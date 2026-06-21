//! The browser engine: owns the pipeline state and produces a painted framebuffer.
//!
//! Phase 0/1 scope: fetch a URL (via `net`), remember the result, and paint a status
//! screen — a computed gradient plus real text rendered by our compositor. The full
//! parse → style → layout → paint pipeline lands in later phases; the function boundaries
//! (`html::parse`, `style`, `layout`) already exist as stubs so wiring them in is additive.

mod canvas;
mod font;
mod svg;

use std::collections::HashMap;
use std::time::Instant;

use font::SystemFont;
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

impl Engine {
    pub fn new() -> Self {
        Engine {
            vp_w: 800,
            vp_h: 600,
            scale: 1.0,
            state: LoadState::Empty,
            font: SystemFont::load(),
            scroll_y: 0.0,
            is_dark: false,
            layout_cache: None,
            framebuffer: None,
            session: None,
            focused_node: None,
            focus_value: None,
            hovered_node: None,
            prev_intersecting: HashMap::new(),
            prev_size: HashMap::new(),
            frame_cb: None,
            selection: None,
            inspect_node: None,
            canvas_bitmaps: HashMap::new(),
            svg_bitmaps: HashMap::new(),
            mask_sources: HashMap::new(),
            mask_bitmaps: HashMap::new(),
        }
    }

    /// Install (or clear, with `None`) the progressive-load frame callback. When set, `load_url`
    /// invokes `cb(ctx, frame_view)` synchronously on the load thread each time it paints a partial
    /// or final frame as HTML streams in. The `FrameView` pixels are valid only for the duration of
    /// the call (the engine reuses its buffer) — the callback must copy synchronously.
    pub fn set_frame_callback(&mut self, cb: Option<(FrameCallback, *mut std::ffi::c_void)>) {
        self.frame_cb = cb;
    }

    /// Scroll the page by `dy` device pixels (positive = down). The upper bound is clamped
    /// against the document height on the next `render`.
    pub fn scroll_by(&mut self, dy: f32) {
        self.scroll_y = (self.scroll_y + dy).max(0.0);
    }

    pub fn set_viewport(&mut self, w: u32, h: u32, scale: f32) {
        self.vp_w = w.max(1);
        self.vp_h = h.max(1);
        self.scale = if scale > 0.0 { scale } else { 1.0 };
        // Surface the real viewport + scale to page JS (window.innerWidth/innerHeight,
        // devicePixelRatio) so responsive/HiDPI code sees true values.
        js::set_device_metrics(self.vp_w, self.vp_h, self.scale);
    }

    /// Set the effective OS appearance (Dark/Light). Pushed by the host on launch and on every
    /// Light/Dark toggle. Surfaces the flag to:
    ///   - page JS: `matchMedia('(prefers-color-scheme: dark)').matches` and a `change` event on
    ///     existing `MediaQueryList`s, and
    ///   - the CSS cascade: `@media (prefers-color-scheme: dark|light)` rules,
    /// then invalidates the layout cache so the next `render` re-cascades with the new appearance.
    pub fn set_color_scheme(&mut self, is_dark: bool) {
        if self.is_dark == is_dark {
            return; // no change → don't churn the layout cache / re-dispatch
        }
        self.is_dark = is_dark;
        // Process-global flags read by the JS worker and the cascade respectively.
        js::set_color_scheme_dark(is_dark);
        style::set_color_scheme_dark(is_dark);
        // Re-cascade on the next render so @media (prefers-color-scheme) rules re-apply.
        self.layout_cache = None;
        // Fire `change` on live MediaQueryLists in the page, adopting any DOM mutations the
        // handlers make so they're reflected on the next render.
        if let Some(session) = &self.session {
            let (mut snapshot, console) = session.notify_color_scheme_changed();
            snapshot.prune_invalid();
            if let LoadState::Loaded {
                doc, console: c, ..
            } = &mut self.state
            {
                *doc = Some(snapshot);
                c.extend(console);
            }
        }
    }

    /// Fetch `url` (streaming) and remember the outcome, painting INCREMENTALLY as the HTML body
    /// arrives so the page appears before the full download finishes. Returns 0 on success, -1 on
    /// network error.
    ///
    /// Single-threaded by design: all streaming, partial parsing/rendering, and frame callbacks run
    /// on the caller's thread inside this call. The caller owns the engine for the whole load and
    /// does not tick/render concurrently.
    ///
    /// Streaming structure:
    /// 1. Reset per-navigation state (scroll/caches/focus/hover/observers/network log).
    /// 2. Feed each network chunk into a [`html::StreamParser`]; throttled to at most every
    ///    [`PARTIAL_PAINT_INTERVAL`], take a partial DOM snapshot, install it as a PARTIAL loaded
    ///    state with INLINE-ONLY styles (no blocking network), paint, and emit a frame.
    /// 3. After the body finishes, run the EXACT same finalize as the non-streaming path
    ///    (`finish` → base_url → `start_session` (V8) → full `collect_stylesheets` (external CSS) →
    ///    `collect_images` → `prune_invalid` → `deliver_observations`), so the FINAL state and frame
    ///    are byte-for-byte what the engine produced before — streaming only ADDS earlier frames.
    pub fn load_url(&mut self, url: &str) -> i32 {
        self.scroll_y = 0.0; // new navigation starts at the top
        self.layout_cache = None; // invalidate cached layout for the previous page
        self.focused_node = None; // a new page has no focused field
        self.focus_value = None;
        self.hovered_node = None; // and nothing is hovered
        self.prev_intersecting.clear(); // observer change-tracking is per-page
        self.prev_size.clear();
        self.session = None; // drop the previous page's runtime (stops its thread)
        self.selection = None; // a new page starts with nothing selected
        self.inspect_node = None; // and nothing highlighted in the Elements inspector
        net::clear_network_log(); // devtools Network tab tracks this navigation's requests

        // Stream the body: re-parse on each chunk and paint throttled partial frames. We also
        // accumulate the raw bytes so the non-HTML branch / content sniffing below can inspect the
        // full body without depending on the streaming parser's internal buffer.
        let mut parser = html::StreamParser::new();
        let mut last_paint: Option<Instant> = None;
        let streaming = self.frame_cb.is_some();
        let result = net::fetch_streaming(url, &mut |chunk| {
            parser.feed(chunk);
            // Partial frames are pure cost when nobody is listening — only paint when a frame
            // callback is installed.
            if !streaming {
                return;
            }
            let now = Instant::now();
            let due = match last_paint {
                Some(t) => now.duration_since(t) >= PARTIAL_PAINT_INTERVAL,
                None => true, // always emit the first partial frame
            };
            if !due {
                return;
            }
            last_paint = Some(now);
            // Partial frame: inline-only styles, no images/console, scripts have NOT run yet.
            let snapshot = parser.snapshot();
            self.install_partial(snapshot, url);
            self.emit_partial_frame();
        });

        match result {
            Ok(meta) => {
                // HTML when the server says so, OR when the type is unknown/generic and the body
                // sniffs as HTML (mirrors the old `content_type.contains("html")` gate, extended
                // with a structural sniff for type-less responses).
                let ct = meta.content_type.to_ascii_lowercase();
                let final_doc = parser.finish();
                let looks_html = ct.contains("html")
                    || (ct.is_empty() || ct == "application/octet-stream")
                        && document_looks_like_html(&final_doc);

                // Build the FINAL state exactly as the non-streaming path did.
                let base = if looks_html {
                    base_url(&final_doc, &meta.final_url)
                } else {
                    meta.final_url.clone()
                };

                let mut console: Vec<String> = Vec::new();
                let doc = if looks_html {
                    // Pre-layout the parsed document (inline-CSS only, no network) so the JS session
                    // can seed `layout_rects` BEFORE its scripts run. Synchronous layout-dependent
                    // reads during load (getBoundingClientRect / elementFromPoint / caret*FromPoint)
                    // then see real geometry instead of 0/null. Best-effort: external CSS/images
                    // aren't loaded yet, and the engine re-lays-out + re-pushes authoritatively after.
                    let initial_rects = {
                        let inline_styles = collect_inline_stylesheets(&final_doc, &base);
                        let no_images: HashMap<dom::NodeId, DecodedImage> = HashMap::new();
                        compute_initial_rects(
                            &final_doc,
                            &inline_styles,
                            &no_images,
                            self.font.as_ref(),
                            self.vp_w,
                            self.vp_h,
                            self.scale,
                            self.is_dark,
                        )
                    };
                    // Start a persistent JS runtime: runs classic scripts + ES modules and stays
                    // alive so event handlers/timers keep working. Returns the initial snapshot.
                    let (session, mut snapshot, sess_console) =
                        start_session(final_doc, &base, initial_rects);
                    console.extend(sess_console);
                    // Page JS can leave stale node ids; drop any out of the arena.
                    snapshot.prune_invalid();
                    self.session = session;
                    Some(snapshot)
                } else {
                    None
                };

                // Collect stylesheets AFTER scripts run (runtime-injected CSS is included), and
                // images after that (script-inserted/mutated `src` are seen). Full external fetches.
                let styles = match &doc {
                    Some(d) => {
                        let (s, style_console) = collect_stylesheets(d, &base);
                        console.extend(style_console);
                        s
                    }
                    None => Vec::new(),
                };
                let images = match &doc {
                    Some(d) => collect_images(d, &base, &mut console),
                    None => HashMap::new(),
                };

                self.state = LoadState::Loaded {
                    url: meta.final_url,
                    doc,
                    styles,
                    console,
                    images,
                };
                self.layout_cache = None; // partial frames left a stale (inline-only) cache
                                          // Build the initial layout and push the rects to the JS Session so the first
                                          // getBoundingClientRect/offsetWidth/scrollHeight reads after load see real geometry.
                {
                    let dw = ((self.vp_w as f32) * self.scale).round().max(1.0) as u32;
                    let dh = ((self.vp_h as f32) * self.scale).round().max(1.0) as u32;
                    if self.ensure_layout(dw, dh, 0.0) {
                        self.push_layout_rects();
                    }
                }
                // Fire the initial IntersectionObserver/ResizeObserver observations.
                self.deliver_observations();
                // Paint the FINAL frame and emit it (identical to the non-streaming render).
                if self.frame_cb.is_some() {
                    self.emit_final_frame();
                }
                0
            }
            Err(e) => {
                self.state = LoadState::Failed {
                    url: url.to_string(),
                    error: e,
                };
                self.layout_cache = None;
                if self.frame_cb.is_some() {
                    self.emit_final_frame();
                }
                -1
            }
        }
    }

    /// Install `doc` as a PARTIAL loaded state for progressive rendering: inline `<style>`-only
    /// stylesheets (NO `<link>`/`@import` network fetches), empty images, empty console. Scripts
    /// have not run. Invalidates the layout cache so the next paint re-cascades against this DOM.
    fn install_partial(&mut self, doc: dom::Document, url: &str) {
        let base = base_url(&doc, url);
        let styles = collect_inline_stylesheets(&doc, &base);
        self.state = LoadState::Loaded {
            url: url.to_string(),
            doc: Some(doc),
            styles,
            console: Vec::new(),
            images: HashMap::new(),
        };
        self.layout_cache = None;
    }

    /// Paint the current state into `self.framebuffer` and hand a borrowed [`FrameView`] to the
    /// installed frame callback. The borrow choreography: `render(&mut self)` finishes (releasing the
    /// `&mut self` borrow) and stores the framebuffer on `self`; we then read the (now immutable)
    /// buffer fields and the `Copy` callback tuple out of `self` and invoke it — so `self` is never
    /// borrowed mutably while we read its framebuffer for the callback.
    fn emit_partial_frame(&mut self) {
        self.render();
        self.dispatch_frame();
    }

    /// Like [`emit_partial_frame`] but named for the terminal paint; identical mechanics.
    fn emit_final_frame(&mut self) {
        self.render();
        self.dispatch_frame();
    }

    /// Read the last-painted framebuffer and forward it to the frame callback (if any). Pulled out
    /// so the `&mut self` render borrow is fully released before we read the buffer for the callback.
    fn dispatch_frame(&mut self) {
        let Some((cb, ctx)) = self.frame_cb else {
            return;
        };
        let view = match self.framebuffer.as_ref() {
            Some(fb) => FrameView {
                pixels: fb.pixels.as_ptr(),
                width: fb.width,
                height: fb.height,
                stride: fb.stride,
            },
            None => FrameView {
                pixels: std::ptr::null(),
                width: 0,
                height: 0,
                stride: 0,
            },
        };
        cb(ctx, view);
    }

    /// Recompute the cascade + layout for the current viewport into `layout_cache`, unless a
    /// cached tree for this exact device size is already present. This is the expensive part of
    /// rendering; keeping it out of the scroll path makes scrolling cheap (paint-only).
    /// Ensure `layout_cache` reflects the device viewport `(dw, dh)`. Returns `true` if the layout
    /// was (re)built this call (so callers can push the fresh rects to the JS Session without
    /// shipping 21k rects on every idle tick — see [`push_layout_rects`]); `false` when the cached
    /// layout was reused unchanged.
    fn ensure_layout(&mut self, dw: u32, dh: u32, header_h: f32) -> bool {
        // Feed the real logical viewport + scale to the cascade so @media (width/height/resolution),
        // @container, and vw/vh units evaluate against the true window — and, since this runs on
        // every viewport change, they re-evaluate on resize.
        style::set_viewport_metrics(self.vp_w as f32, self.vp_h as f32, self.scale);
        // The OS appearance (for `@media (prefers-color-scheme)` and the `color-scheme` resolution)
        // is fed to the cascade via `cascade_with_root_scheme(.., self.is_dark)` below — set there
        // under the cascade lock so it's atomic with the cascade (no separate global write here,
        // which would race a concurrent cascade reading the flag).
        // Feed pointer/keyboard interaction state to the cascade so `:hover`/`:focus`/… match.
        style::set_interaction_state(
            self.hovered_node.map(|n| n.0),
            self.focused_node.map(|n| n.0),
        );
        if matches!(&self.layout_cache, Some(c) if c.dw == dw && c.dh == dh) {
            return false;
        }
        // Compute into owned values first so the `&self.state` borrow ends before we assign.
        let computed = if let (
            Some(font),
            LoadState::Loaded {
                doc: Some(d),
                styles,
                console,
                images,
                ..
            },
        ) = (self.font.as_ref(), &self.state)
        {
            // The page always uses the full framebuffer height; the console now lives in the
            // Swift devtools panel, not painted by the engine.
            let _ = console;
            let page_max_y = dh as f32;
            let vw = (dw as f32).max(1.0);
            let vh = (page_max_y - header_h).max(1.0);
            let measurer = FontMeasurer { font };
            let mut intrinsic_sizes: HashMap<dom::NodeId, (f32, f32)> = images
                .iter()
                .map(|(&id, img)| (id, (img.w as f32, img.h as f32)))
                .collect();
            // <canvas> intrinsic size = its width/height attributes (default 300x150). Layout's
            // canvas branch reads attrs directly too, but seeding this keeps aspect-ratio scaling
            // (one CSS dimension set) consistent with how <img> is handled.
            collect_canvas_intrinsics(d, &mut intrinsic_sizes);
            // Inline <svg> is a replaced element too: its intrinsic size is its width/height attrs
            // (or its viewBox w/h, else 300x150). The engine rasterizes the SVG subtree to a bitmap.
            collect_svg_intrinsics(d, &mut intrinsic_sizes);
            let (computed, root_scheme_dark) =
                style::cascade_with_root_scheme(d, styles, self.is_dark);
            let root = layout::layout_document(
                d,
                &computed,
                vw,
                vh,
                &measurer,
                &intrinsic_sizes,
                self.focused_node,
            );
            let content_h = root.dimensions.margin_box().height;
            Some((root, content_h, root_scheme_dark))
        } else {
            None
        };
        self.layout_cache = computed.map(|(root, content_h, root_scheme_dark)| LayoutCache {
            dw,
            dh,
            root,
            content_h,
            root_scheme_dark,
        });
        true
    }

    /// Rebuild [`Self::canvas_bitmaps`] from the JS 2D-context display lists. Pulls every canvas's
    /// `{id,width,height,commands}` via the Session, parses the JSON, and rasterizes each command
    /// stream into an RGBA bitmap. Guarded: returns immediately if there's no script Session or the
    /// loaded DOM contains no `<canvas>` (the common case), so non-canvas pages pay nothing.
    fn update_canvas_bitmaps(&mut self) {
        let session = match &self.session {
            Some(s) => s,
            None => return,
        };
        // Guard: skip the JS round-trip unless the DOM actually has a <canvas>.
        let has_canvas = matches!(&self.state, LoadState::Loaded { doc: Some(d), .. }
            if (0..d.len()).any(|i| matches!(&d.get(dom::NodeId(i)).data,
                dom::NodeData::Element(e) if e.tag.eq_ignore_ascii_case("canvas"))));
        if !has_canvas {
            if !self.canvas_bitmaps.is_empty() {
                self.canvas_bitmaps.clear();
            }
            return;
        }
        let json = session.canvas_lists();
        let font = self.font.as_ref();
        // Source pixels for drawImage: decoded <img> bitmaps (by node id) plus PREVIOUS-frame canvas
        // bitmaps (canvas-draws-canvas uses last frame — a one-frame lag, acceptable).
        let empty_images: HashMap<dom::NodeId, DecodedImage> = HashMap::new();
        let images: &HashMap<dom::NodeId, DecodedImage> = match &self.state {
            LoadState::Loaded { images, .. } => images,
            _ => &empty_images,
        };
        let mut sources: HashMap<usize, (&[u8], u32, u32)> = HashMap::new();
        for (id, img) in images.iter() {
            sources.insert(id.0, (img.rgba.as_slice(), img.w, img.h));
        }
        for (id, img) in self.canvas_bitmaps.iter() {
            sources
                .entry(id.0)
                .or_insert((img.rgba.as_slice(), img.w, img.h));
        }
        let mut next: HashMap<dom::NodeId, DecodedImage> = HashMap::new();
        for cv in canvas::parse_canvas_lists(&json) {
            let bmp = canvas::rasterize_canvas(&cv, font, &sources);
            next.insert(dom::NodeId(cv.id), bmp);
        }
        // Push the freshly-rasterized pixels back to the JS Session so getImageData reads real RGBA
        // (one-render lag — the next getImageData call sees these). Fire-and-forget.
        let pixels: Vec<(usize, u32, u32, Vec<u8>)> = next
            .iter()
            .map(|(id, img)| (id.0, img.w, img.h, img.rgba.clone()))
            .collect();
        session.set_canvas_pixels(pixels);
        self.canvas_bitmaps = next;
    }

    /// Rebuild [`Self::svg_bitmaps`] by walking each inline `<svg>` DOM subtree and rasterizing it
    /// into an RGBA bitmap (composited below exactly like a decoded `<img>` / canvas). Rasterizes at
    /// the laid-out content-box's device size (so the SVG is crisp at any scale), falling back to the
    /// element's intrinsic size when no layout box exists yet. Guarded: pages with no `<svg>` clear
    /// the cache and pay nothing. Call AFTER `ensure_layout` so box rects are available.
    fn update_svg_bitmaps(&mut self) {
        let doc = match &self.state {
            LoadState::Loaded { doc: Some(d), .. } => d,
            _ => {
                if !self.svg_bitmaps.is_empty() {
                    self.svg_bitmaps.clear();
                }
                return;
            }
        };
        // Collect every <svg> element's node id (top-level svgs only render once; nested ones are
        // drawn by their ancestor's walk, but rasterizing them standalone is harmless).
        let svg_ids: Vec<dom::NodeId> = (0..doc.len())
            .map(dom::NodeId)
            .filter(|&id| {
                matches!(&doc.get(id).data,
                dom::NodeData::Element(e) if e.tag.eq_ignore_ascii_case("svg"))
            })
            .collect();
        if svg_ids.is_empty() {
            if !self.svg_bitmaps.is_empty() {
                self.svg_bitmaps.clear();
            }
            return;
        }
        // Laid-out content-box pixel sizes (device px) keyed by node id, for crisp rasterization.
        let mut rects: HashMap<usize, layout::Rect> = HashMap::new();
        if let Some(cache) = &self.layout_cache {
            collect_content_rects(&cache.root, &mut rects);
        }
        let font = self.font.as_ref();
        let mut next: HashMap<dom::NodeId, DecodedImage> = HashMap::new();
        for id in svg_ids {
            let el = match &doc.get(id).data {
                dom::NodeData::Element(e) => e,
                _ => continue,
            };
            let (iw, ih) = svg::intrinsic_size(el);
            let (mut w, mut h) = match rects.get(&id.0) {
                Some(r) if r.width >= 1.0 && r.height >= 1.0 => (r.width, r.height),
                // No box yet: rasterize at intrinsic size × scale.
                _ => (iw * self.scale, ih * self.scale),
            };
            w = w.round().clamp(1.0, 4096.0);
            h = h.round().clamp(1.0, 4096.0);
            let bmp = svg::rasterize_svg(doc, id, w as u32, h as u32, font);
            next.insert(id, bmp);
        }
        self.svg_bitmaps = next;
    }

    /// Build a per-box `mask-image` coverage bitmap for every masked element (the icon technique:
    /// `background: currentColor; mask: url(icon.svg) ... / contain`). Walks the laid-out tree for
    /// boxes whose `extras.mask_image` is set, fetches/decodes each distinct mask source once
    /// (cached in `self.mask_sources` keyed by url), then rasterizes the source to the box's
    /// border-box device size honoring `contain`/`cover`/stretch. The result's ALPHA channel is the
    /// mask coverage the painter multiplies the background by. Guarded: mask-free pages clear the
    /// caches and pay nothing. Call AFTER `ensure_layout`.
    fn update_mask_bitmaps(&mut self) {
        // Gather (node, border-box device rect, MaskImage) for every masked box in the layout tree.
        let mut targets: Vec<(dom::NodeId, layout::Rect, style::MaskImage)> = Vec::new();
        if let Some(cache) = &self.layout_cache {
            collect_mask_targets(&cache.root, &mut targets);
        }
        if targets.is_empty() {
            if !self.mask_bitmaps.is_empty() {
                self.mask_bitmaps.clear();
            }
            if !self.mask_sources.is_empty() {
                self.mask_sources.clear();
            }
            return;
        }

        // Mask `url(...)`s are resolved against their owning stylesheet's base during the cascade
        // (see `style::apply_declaration`), so `mask.url` is normally already absolute. The page
        // URL here is a FALLBACK for masks whose base was unknown (inline `style=""`, or a sheet
        // parsed without a base) — `load_mask_source` joins it, which leaves an absolute url intact.
        let base = match &self.state {
            LoadState::Loaded { url, .. } => url.clone(),
            _ => String::new(),
        };

        // Fetch + decode any mask sources we haven't seen yet, keyed by their resolved url.
        for (_, _, mask) in &targets {
            if self.mask_sources.contains_key(&mask.url) {
                continue;
            }
            if let Some(src) = load_mask_source(&mask.url, &base) {
                self.mask_sources.insert(mask.url.clone(), src);
            }
        }
        // Drop cached sources no longer referenced by the page.
        let live: std::collections::HashSet<&String> =
            targets.iter().map(|(_, _, m)| &m.url).collect();
        self.mask_sources.retain(|k, _| live.contains(k));

        let font = self.font.as_ref();
        let mut next: HashMap<dom::NodeId, DecodedImage> = HashMap::new();
        for (id, rect, mask) in targets {
            let src = match self.mask_sources.get(&mask.url) {
                Some(s) => s,
                None => continue, // fetch/decode failed
            };
            let w = rect.width.round().clamp(1.0, 4096.0) as u32;
            let h = rect.height.round().clamp(1.0, 4096.0) as u32;
            let cov = rasterize_mask_coverage(src, w, h, mask.size, font);
            next.insert(id, cov);
        }
        self.mask_bitmaps = next;
    }

    /// Push the freshly-built layout to the JS Session so `getBoundingClientRect()` /
    /// `offsetWidth` / `scrollHeight` etc. return real values. Converts the engine's
    /// document-absolute, top-origin **device-px** border-box rects to **CSS px** (÷ scale) and
    /// fires them at the Session worker (fire-and-forget — no reply). Callers gate this on
    /// "layout was actually rebuilt this frame" to avoid shipping the whole rect table every tick.
    ///
    /// Coordinate contract (all CSS px): rects are document-absolute top-origin; `scroll_y_css` is
    /// the vertical scroll offset; `doc_height_css` is the full content height. The Session makes
    /// `getBoundingClientRect` viewport-relative by subtracting `scroll_y_css` itself.
    fn push_layout_rects(&self) {
        let (session, cache) = match (&self.session, &self.layout_cache) {
            (Some(s), Some(c)) => (s, c),
            _ => return,
        };
        let mut rects: HashMap<usize, layout::Rect> = HashMap::new();
        collect_node_rects(&cache.root, &mut rects);
        let inv = if self.scale > 0.0 {
            1.0 / self.scale
        } else {
            1.0
        };
        let list: Vec<(usize, f32, f32, f32, f32)> = rects
            .iter()
            .map(|(&id, r)| (id, r.x * inv, r.y * inv, r.width * inv, r.height * inv))
            .collect();
        // Decoded intrinsic size of each <img>, for img.naturalWidth/naturalHeight. The decoded
        // bitmap's w/h are stored in CSS px (the same values layout seeds into intrinsic_sizes).
        let naturals: Vec<(usize, f32, f32)> = match &self.state {
            LoadState::Loaded { images, .. } => images
                .iter()
                .map(|(id, img)| (id.0, img.w as f32, img.h as f32))
                .collect(),
            _ => Vec::new(),
        };
        // CSSOM used inset values per positioned box (device px -> CSS px), for getComputedStyle's
        // resolved value of top/right/bottom/left when the element has a box.
        let mut insets: HashMap<usize, [f32; 4]> = HashMap::new();
        collect_used_insets(&cache.root, &mut insets);
        let inset_list: Vec<(usize, f32, f32, f32, f32)> = insets
            .iter()
            .map(|(&id, v)| (id, v[0] * inv, v[1] * inv, v[2] * inv, v[3] * inv))
            .collect();
        // CSSOM used margin values (device px -> CSS px), for getComputedStyle's resolved margins.
        let mut margins: HashMap<usize, [f32; 4]> = HashMap::new();
        collect_used_margins(&cache.root, &mut margins);
        let margin_list: Vec<(usize, f32, f32, f32, f32)> = margins
            .iter()
            .map(|(&id, v)| (id, v[0] * inv, v[1] * inv, v[2] * inv, v[3] * inv))
            .collect();
        let scroll_y_css = self.scroll_y * inv;
        let doc_height_css = cache.content_h * inv;
        session.set_layout_rects(
            list,
            naturals,
            inset_list,
            margin_list,
            scroll_y_css,
            doc_height_css,
        );
    }

    /// Paint the current state into a fresh framebuffer and return a reference to it.
    pub fn render(&mut self) -> &Framebuffer {
        let dw = ((self.vp_w as f32) * self.scale).round().max(1.0) as u32;
        let dh = ((self.vp_h as f32) * self.scale).round().max(1.0) as u32;
        // No engine inset: page paints flush at (0,0); margin/padding come from CSS.
        let header_h = 0.0;

        // Expensive: cascade + layout (cached across scrolls / repeated renders at this size).
        // When the layout was actually (re)built (first paint, viewport resize, or a DOM mutation
        // invalidated the cache), push the fresh rects to the JS Session so element-geometry reads
        // stay current. Gated on the rebuild so scroll-only repaints don't re-ship the rect table.
        if self.ensure_layout(dw, dh, header_h) {
            self.push_layout_rects();
        }

        // Pull each <canvas>'s JS display list and rasterize it into a bitmap (composited below
        // exactly like a decoded <img>). Guarded so script-free / canvas-free pages pay nothing.
        self.update_canvas_bitmaps();
        // Rasterize each inline <svg> subtree to a bitmap (also composited like a decoded <img>).
        // After ensure_layout so each SVG's content-box device size is known for crisp output.
        self.update_svg_bitmaps();
        // Build per-box mask coverage bitmaps (the `mask-image` icon technique). After ensure_layout
        // so each masked box's border-box device size is known for the contain/cover fit.
        self.update_mask_bitmaps();

        let mut fb = Framebuffer::new(dw, dh);
        let mut scroll_y = self.scroll_y;

        // A loaded HTML page paints on a real document canvas (white by default, or the html/body
        // background). The splash / non-HTML / error states (no layout tree) keep the chrome gradient.
        match (&self.state, &self.layout_cache) {
            (LoadState::Loaded { .. }, Some(cache)) => {
                fb.clear(page_background(&cache.root, cache.root_scheme_dark))
            }
            _ => paint_gradient(&mut fb),
        }

        let px = 16.0 * self.scale;
        if let Some(font) = self.font.as_ref() {
            match &self.state {
                LoadState::Empty => {
                    draw_text(
                        &mut fb,
                        font,
                        "browser — phase 2",
                        12.0 * self.scale,
                        19.0 * self.scale,
                        13.0 * self.scale,
                        Color::rgb(120, 200, 255),
                    );
                    draw_text(
                        &mut fb,
                        font,
                        "Enter a URL and press Go.",
                        12.0 * self.scale,
                        60.0 * self.scale,
                        px,
                        Color::WHITE,
                    );
                }
                LoadState::Loaded {
                    url, doc, images, ..
                } => {
                    let left = 0.0;
                    // The page fills the full framebuffer height; the console panel is rendered by
                    // the Swift devtools, not the engine.
                    let page_max_y = dh as f32;
                    let viewport_height = (page_max_y - header_h).max(1.0);

                    if let Some(cache) = &self.layout_cache {
                        // Scroll just re-paints the cached layout at a new offset.
                        let max_scroll = (cache.content_h - viewport_height).max(0.0);
                        scroll_y = scroll_y.min(max_scroll);
                        // Resolve the selection (if any) into a per-text-run highlight range, in the
                        // same DFS order the painter visits text runs. A running counter in the
                        // painter indexes into this so each run highlights its selected sub-range.
                        let sel_ranges = if self.selection.is_some() {
                            let runs = collect_text_runs(&cache.root);
                            self.selection_ranges(&runs)
                        } else {
                            Vec::new()
                        };
                        let mut run_idx = 0usize;
                        paint_box(
                            &mut fb,
                            font,
                            &cache.root,
                            left,
                            header_h - scroll_y,
                            header_h,
                            page_max_y,
                            images,
                            &self.canvas_bitmaps,
                            &self.svg_bitmaps,
                            &self.mask_bitmaps,
                            &sel_ranges,
                            &mut run_idx,
                        );
                        // Overlay scrollbar on the right edge when the page overflows the viewport.
                        paint_scrollbar(
                            &mut fb,
                            header_h,
                            viewport_height,
                            cache.content_h,
                            scroll_y,
                            self.scale,
                        );
                    } else if doc.is_none() {
                        draw_text(
                            &mut fb,
                            font,
                            &format!("(non-HTML content: {})", url),
                            left,
                            header_h + px * 1.4,
                            px,
                            Color::WHITE,
                        );
                    }
                }
                LoadState::Failed { url, error } => {
                    draw_text(
                        &mut fb,
                        font,
                        "browser — phase 2",
                        12.0 * self.scale,
                        19.0 * self.scale,
                        13.0 * self.scale,
                        Color::rgb(120, 200, 255),
                    );
                    let baseline = 60.0 * self.scale;
                    draw_text(
                        &mut fb,
                        font,
                        &format!("Failed: {url}"),
                        16.0 * self.scale,
                        baseline,
                        px,
                        Color::rgb(255, 120, 120),
                    );
                    draw_text(
                        &mut fb,
                        font,
                        error,
                        16.0 * self.scale,
                        baseline + px * 1.4,
                        px,
                        Color::rgb(255, 180, 180),
                    );
                }
            }
        }

        // DevTools "Elements" inspector overlay: AFTER the page, draw a translucent fill + 1px
        // outline over the highlighted node's border box (document→screen by subtracting scroll_y,
        // matching the rest of paint). Only when a node is set and it has a laid-out rect.
        if let Some(node) = self.inspect_node {
            if let Some(cache) = &self.layout_cache {
                let mut rects: HashMap<usize, layout::Rect> = HashMap::new();
                collect_node_rects(&cache.root, &mut rects);
                if let Some(r) = rects.get(&node.0) {
                    let x = r.x.round() as i32;
                    let y = (r.y - scroll_y).round() as i32;
                    let w = r.width.round().max(0.0) as i32;
                    let h = r.height.round().max(0.0) as i32;
                    if w > 0 && h > 0 {
                        let fill = Color {
                            r: 90,
                            g: 160,
                            b: 255,
                            a: 64,
                        }; // rgba(90,160,255,0.25)
                        let line = Color {
                            r: 90,
                            g: 160,
                            b: 255,
                            a: 230,
                        }; // rgba(90,160,255,0.9)
                        fb.fill_rect(Rect { x, y, w, h }, fill);
                        // 1px solid outline around the border box.
                        fb.fill_rect(Rect { x, y, w, h: 1 }, line);
                        fb.fill_rect(
                            Rect {
                                x,
                                y: y + h - 1,
                                w,
                                h: 1,
                            },
                            line,
                        );
                        fb.fill_rect(Rect { x, y, w: 1, h }, line);
                        fb.fill_rect(
                            Rect {
                                x: x + w - 1,
                                y,
                                w: 1,
                                h,
                            },
                            line,
                        );
                    }
                }
            }
        }

        self.scroll_y = scroll_y; // persist the clamped offset
        self.framebuffer = Some(fb);
        self.framebuffer.as_ref().unwrap()
    }

    /// Borrow the last-rendered framebuffer, if any.
    pub fn framebuffer(&self) -> Option<&Framebuffer> {
        self.framebuffer.as_ref()
    }

    /// V8 heap used by this tab's JS, in bytes (0 if no live session). For the tab tooltip.
    pub fn heap_bytes(&self) -> u64 {
        self.session.as_ref().map(|s| s.heap_bytes()).unwrap_or(0)
    }

    /// Cumulative active JS time on this tab's thread, in nanoseconds (0 if no session). The UI
    /// samples deltas over wall-clock to display a CPU %.
    pub fn cpu_ns(&self) -> u64 {
        self.session.as_ref().map(|s| s.cpu_ns()).unwrap_or(0)
    }

    /// The page's `<title>` text (whitespace-collapsed), if the loaded page has one.
    pub fn title(&self) -> Option<String> {
        let doc = match &self.state {
            LoadState::Loaded { doc: Some(d), .. } => d,
            _ => return None,
        };
        fn find(doc: &dom::Document, id: dom::NodeId) -> Option<String> {
            if let dom::NodeData::Element(e) = &doc.get(id).data {
                if e.tag.eq_ignore_ascii_case("title") {
                    let mut s = String::new();
                    for &c in &doc.get(id).children {
                        if let dom::NodeData::Text(t) = &doc.get(c).data {
                            s.push_str(t);
                        }
                    }
                    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !s.is_empty() {
                        return Some(s);
                    }
                }
            }
            for &c in &doc.get(id).children {
                if let Some(t) = find(doc, c) {
                    return Some(t);
                }
            }
            None
        }
        find(doc, doc.root())
    }

    /// Hit-test the painted page at framebuffer device-pixel `(x, y)` and, if the deepest box
    /// hit belongs to (or descends from) an `<a href>`, return the absolute link URL.
    ///
    /// Coordinate mapping mirrors `render`/`paint_box`: page content is painted at
    /// `(left, header_h - scroll_y)`, so we invert that to get layout coordinates. Returns `None`
    /// when there's no cached layout, no DOM, no box hit, no enclosing link, or the href can't be
    /// resolved to a fetchable absolute URL (in-page `#frag` / `javascript:` are rejected by
    /// `resolve_url`).
    pub fn link_at(&self, x: f32, y: f32) -> Option<String> {
        // SAME constants as render/paint_box (no engine inset).
        let left = 0.0;
        let header_h = 0.0;

        let cache = self.layout_cache.as_ref()?;
        let (doc, page_url) = match &self.state {
            LoadState::Loaded {
                doc: Some(d), url, ..
            } => (d, url),
            _ => return None,
        };

        // Device pixels -> layout coordinates.
        let lx = x - left;
        let ly = y - (header_h - self.scroll_y);

        // Find the deepest box containing the point that carries a DOM node.
        let node = deepest_node_at(&cache.root, lx, ly)?;

        // Walk up the DOM to the nearest ancestor-or-self <a> with a non-empty href.
        let mut cur = Some(node);
        while let Some(id) = cur {
            if let dom::NodeData::Element(el) = &doc.get(id).data {
                if el.tag.eq_ignore_ascii_case("a") {
                    if let Some(href) = el.attrs.get("href") {
                        if !href.trim().is_empty() {
                            return resolve_url(page_url, href);
                        }
                    }
                }
            }
            cur = doc.get(id).parent;
        }
        None
    }

    /// Dispatch a synthetic `click` (device pixel coords, viewport-relative) into the live page
    /// JS: hit-tests the cached layout for the deepest node, fires the page's `click` handlers
    /// (with bubbling) in the persistent runtime, then replaces the rendered DOM with the updated
    /// snapshot and invalidates the layout cache. Returns `true` if a re-render is warranted.
    /// Dispatch a raw mouse event of `kind` (e.g. "mousedown", "mouseup", "dblclick",
    /// "contextmenu") to the node under `(x, y)` (device px), with bubbling — no focus/toggle/submit
    /// side effects (those are `dispatch_click`'s job). Returns whether a re-render is warranted.
    pub fn dispatch_mouse(&mut self, kind: &str, x: f32, y: f32) -> bool {
        let node = match self.layout_cache.as_ref() {
            Some(cache) => match deepest_node_at(&cache.root, x, y + self.scroll_y) {
                Some(n) => n,
                None => return false,
            },
            None => return false,
        };
        let session = match &self.session {
            Some(s) => s,
            None => return false,
        };
        let cx = (x / self.scale) as f64;
        let cy = (y / self.scale) as f64;
        let (mut snapshot, console) = session.dispatch_event(node.0, kind, cx, cy);
        snapshot.prune_invalid();
        if let LoadState::Loaded {
            doc, console: c, ..
        } = &mut self.state
        {
            *doc = Some(snapshot);
            c.extend(console);
            self.layout_cache = None;
            true
        } else {
            false
        }
    }

    pub fn dispatch_click(&mut self, x: f32, y: f32) -> bool {
        // Hit-test in layout (document) coordinates: header_h = 0, left = 0, so add scroll_y.
        let node = match self.layout_cache.as_ref() {
            Some(cache) => match deepest_node_at(&cache.root, x, y + self.scroll_y) {
                Some(n) => n,
                None => return false,
            },
            None => return false,
        };
        let session = match &self.session {
            Some(s) => s,
            None => return false,
        };
        // clientX/clientY are logical (CSS) px relative to the viewport.
        let cx = (x / self.scale) as f64;
        let cy = (y / self.scale) as f64;
        let (mut snapshot, mut console) = session.dispatch_event(node.0, "click", cx, cy);
        snapshot.prune_invalid();

        // New text focus: the nearest ancestor-or-self of the hit node that is an editable text
        // field (text-like <input> / <textarea>), else clear focus. Computed against the new
        // snapshot so the node ids are valid in the doc we're about to store.
        let new_focus = editable_text_ancestor(&snapshot, node);
        let session = self.session.as_ref().unwrap();

        // Focus transition: if focus moved, fire blur (+ change if the old field's value changed)
        // on the old field, then focus on the new field. focus/blur do not bubble.
        if self.focused_node != new_focus {
            if let Some(old) = self.focused_node {
                if old.0 < snapshot.len() {
                    // change fires first (bubbles) when the value differs from focus time.
                    let cur_val = node_value(&snapshot, old);
                    if self.focus_value.is_some()
                        && self.focus_value.as_deref() != cur_val.as_deref()
                    {
                        let (s, c) = session.fire_event(old.0, "change");
                        snapshot = s;
                        snapshot.prune_invalid();
                        console.extend(c);
                    }
                    let (s, c) = session.fire_event_nonbubbling(old.0, "blur");
                    snapshot = s;
                    snapshot.prune_invalid();
                    console.extend(c);
                }
            }
            if let Some(newf) = new_focus {
                let (s, c) = session.fire_event_nonbubbling(newf.0, "focus");
                snapshot = s;
                snapshot.prune_invalid();
                console.extend(c);
            }
            self.focus_value = new_focus.and_then(|n| node_value(&snapshot, n));
        }
        self.focused_node = new_focus;

        // Checkbox / radio toggle: if the click landed on (or inside, e.g. a <label for>) a
        // checkable input that isn't disabled, toggle it (fires input + change).
        if let Some(target) = checkable_target(&snapshot, node) {
            let (s, c) = session.toggle_checkbox(target.0);
            snapshot = s;
            snapshot.prune_invalid();
            console.extend(c);
        }

        // <details>/<summary>: a click on a summary toggles the parent <details> open/closed.
        if let Some(details) = details_toggle_target(&snapshot, node) {
            let (s, c) = session.toggle_details(details.0);
            snapshot = s;
            snapshot.prune_invalid();
            console.extend(c);
        }

        // Submit: a click on a submit button (<input type=submit>, <button type=submit>, or a
        // <button> with no type) inside a form fires `submit` on the nearest ancestor <form>.
        if let Some(form) = submit_target_form(&snapshot, node) {
            let (s, c) = session.fire_event(form.0, "submit");
            snapshot = s;
            snapshot.prune_invalid();
            console.extend(c);
        }

        if let LoadState::Loaded {
            doc, console: c, ..
        } = &mut self.state
        {
            *doc = Some(snapshot);
            c.extend(console);
            self.layout_cache = None; // DOM may have changed → re-cascade/layout/paint
            self.apply_pending_scroll(); // a click handler may have called scrollTo/scrollIntoView
            true
        } else {
            false
        }
    }

    /// Hit-test the cached layout at framebuffer device-pixel `(x, y)` (viewport-relative) and, if
    /// the deepest box hit belongs to (or descends from) a `<select>`, return a [`SelectHit`] with
    /// the select's option labels, the currently-selected index, and the select's on-screen rect
    /// (DEVICE px, scroll already subtracted) so the platform shell can pop up a native dropdown.
    /// Returns `None` when there's no cached layout/DOM, no box hit, or no enclosing `<select>`.
    pub fn select_at(&self, x: f32, y: f32) -> Option<SelectHit> {
        let cache = self.layout_cache.as_ref()?;
        let doc = match &self.state {
            LoadState::Loaded { doc: Some(d), .. } => d,
            _ => return None,
        };
        // Same hit-test mapping as dispatch_click: layout coords add scroll_y.
        let node = deepest_node_at(&cache.root, x, y + self.scroll_y)?;

        // Nearest ancestor-or-self <select>.
        let mut cur = Some(node);
        let select_id = loop {
            let id = cur?;
            if id.0 < doc.len() {
                if let dom::NodeData::Element(el) = &doc.get(id).data {
                    if el.tag.eq_ignore_ascii_case("select") && !el.attrs.contains_key("disabled") {
                        break id;
                    }
                }
            }
            cur = doc.get(id).parent;
        };

        // Collect descendant <option>s (depth-first, including inside <optgroup>).
        let options = collect_options(doc, select_id);
        if options.is_empty() {
            return None;
        }
        let labels: Vec<String> = options.iter().map(|&o| option_text(doc, o)).collect();
        let selected = selected_option_index(doc, select_id, &options);

        // The select's principal box rect (device px), viewport-relative.
        let mut rects: HashMap<usize, layout::Rect> = HashMap::new();
        collect_node_rects(&cache.root, &mut rects);
        let r = rects.get(&select_id.0)?;
        Some(SelectHit {
            node_id: select_id.0,
            x: fnum(r.x),
            y: fnum(r.y - self.scroll_y),
            width: fnum(r.width),
            height: fnum(r.height),
            options: labels,
            selected,
        })
    }

    /// Pick the `index`-th `<option>` of the `<select>` `node_id`: marks it selected (clearing the
    /// others), updates the select's `value`, and fires bubbling `input` then `change` through the
    /// live JS session so the page's handlers run. Adopts the updated DOM snapshot and invalidates
    /// the layout cache (mirrors the checkbox-toggle path). Returns whether the selection changed.
    pub fn set_select_index(&mut self, node_id: usize, index: usize) -> bool {
        let session = match &self.session {
            Some(s) => s,
            None => return false,
        };
        let (changed, mut snapshot, console) = session.set_select_index(node_id, index);
        snapshot.prune_invalid();
        if let LoadState::Loaded {
            doc, console: c, ..
        } = &mut self.state
        {
            *doc = Some(snapshot);
            c.extend(console);
            self.layout_cache = None; // selection changed the DOM → re-cascade/layout/paint
        }
        changed
    }

    /// Begin a text selection at viewport-relative device pixel `(x, y)`: set the anchor (and the
    /// focus, so it starts collapsed) to that DOCUMENT-space point. The caller passes the SAME
    /// pre-scroll coordinates it would pass to [`dispatch_click`]; the engine folds in `scroll_y`
    /// here so the stored point is in document space and stays valid as the page scrolls.
    pub fn selection_start(&mut self, x: f32, y: f32) {
        let p = Point {
            x,
            y: y + self.scroll_y,
        };
        self.selection = Some((p, p));
    }

    /// Extend the active selection's focus to viewport-relative device pixel `(x, y)` (document
    /// space after folding in `scroll_y`), keeping the anchor fixed. No-op if no selection exists.
    pub fn selection_extend(&mut self, x: f32, y: f32) {
        let p = Point {
            x,
            y: y + self.scroll_y,
        };
        if let Some((anchor, _)) = self.selection {
            self.selection = Some((anchor, p));
        } else {
            self.selection = Some((p, p));
        }
    }

    /// Clear any active text selection.
    pub fn selection_clear(&mut self) {
        self.selection = None;
    }

    /// Whether there is a non-empty text selection (anchor and focus resolve to different text
    /// positions). A collapsed selection (a bare click, no drag) reports `false`.
    pub fn has_selection(&self) -> bool {
        !self.selected_text().is_empty()
    }

    /// Resolve the current selection (if any) into a per-text-run highlight range: a vector parallel
    /// to [`collect_text_runs`]'s output where entry `i` is `Some((start_char, end_char))` if run `i`
    /// has selected characters in `[start_char, end_char)`, else `None`. Empty vec when there is no
    /// (non-collapsed) selection. The painter walks text runs in the same DFS order and consults this.
    fn selection_ranges(&self, runs: &[TextRun]) -> Vec<Option<(usize, usize)>> {
        let (a, f) = match self.selection {
            Some(s) => s,
            None => return Vec::new(),
        };
        let font = match self.font.as_ref() {
            Some(font) => font,
            None => return Vec::new(),
        };
        if runs.is_empty() {
            return Vec::new();
        }
        let pa = resolve_text_position(runs, font, a);
        let pf = resolve_text_position(runs, font, f);
        let (start, end) = if pa <= pf { (pa, pf) } else { (pf, pa) };
        if start == end {
            return Vec::new();
        }
        let mut out = vec![None; runs.len()];
        for (ri, slot) in out.iter_mut().enumerate() {
            if ri < start.0 || ri > end.0 {
                continue;
            }
            let len = runs[ri].text.chars().count();
            let s = if ri == start.0 { start.1 } else { 0 };
            let e = if ri == end.0 { end.1 } else { len };
            let s = s.min(len);
            let e = e.min(len);
            if s < e {
                *slot = Some((s, e));
            }
        }
        out
    }

    /// The selected text of the current selection, resolved against the cached layout: the anchor
    /// and focus document points are mapped to (run, char) text positions, ordered, and the runs
    /// between them concatenated (runs on different lines joined with a newline). Empty when there
    /// is no selection, no layout, or the selection is collapsed (zero-length).
    pub fn selected_text(&self) -> String {
        let (a, f) = match self.selection {
            Some(s) => s,
            None => return String::new(),
        };
        let cache = match self.layout_cache.as_ref() {
            Some(c) => c,
            None => return String::new(),
        };
        let font = match self.font.as_ref() {
            Some(font) => font,
            None => return String::new(),
        };
        let runs = collect_text_runs(&cache.root);
        if runs.is_empty() {
            return String::new();
        }
        let pa = resolve_text_position(&runs, font, a);
        let pf = resolve_text_position(&runs, font, f);
        // Order start <= end in (run, char) linear order.
        let (start, end) = if pa <= pf { (pa, pf) } else { (pf, pa) };
        if start == end {
            return String::new();
        }

        let mut out = String::new();
        for ri in start.0..=end.0 {
            let run = &runs[ri];
            let chars: Vec<char> = run.text.chars().collect();
            let s = if ri == start.0 { start.1 } else { 0 };
            let e = if ri == end.0 { end.1 } else { chars.len() };
            let s = s.min(chars.len());
            let e = e.min(chars.len());
            if s >= e {
                continue;
            }
            if !out.is_empty() {
                // Join consecutive runs: a newline when the next run sits on a lower line (its top
                // is clearly below the previous run's top), otherwise a space. This approximates
                // paragraph/line breaks without true bidi/line-box reconstruction.
                let prev = &runs[ri - 1];
                if run.rect.y > prev.rect.y + prev.rect.height * 0.5 {
                    out.push('\n');
                } else {
                    out.push(' ');
                }
            }
            out.extend(&chars[s..e]);
        }
        out
    }

    /// Dispatch a synthetic pointer move (device pixel coords, viewport-relative) into the live page
    /// JS. Hit-tests the deepest node under the pointer; if it changed since the last move, fires
    /// `mouseout`/`mouseleave` on the old node and `mouseover`/`mouseenter`/`mousemove` on the new
    /// one, adopts the updated snapshot, and invalidates the layout cache. Returns `true` if the
    /// hovered node changed (a re-render may be warranted); `false` (cheap no-op) if unchanged.
    pub fn dispatch_move(&mut self, x: f32, y: f32) -> bool {
        let node = match self.layout_cache.as_ref() {
            Some(cache) => deepest_node_at(&cache.root, x, y + self.scroll_y),
            None => None,
        };
        // Unchanged target: no-op (hover stays cheap; we avoid per-pixel churn).
        if node == self.hovered_node {
            return false;
        }
        let session = match &self.session {
            Some(s) => s,
            None => {
                self.hovered_node = node;
                return false;
            }
        };
        let cx = (x / self.scale) as f64;
        let cy = (y / self.scale) as f64;

        let old = self.hovered_node;
        let mut snapshot: Option<dom::Document> = None;
        let mut console: Vec<String> = Vec::new();
        let mut run = |s: &js::Session, id: usize, kind: &str, bubbles: bool| {
            let (mut snap, c) = if bubbles {
                s.dispatch_event(id, kind, cx, cy)
            } else {
                s.fire_event_nonbubbling(id, kind)
            };
            snap.prune_invalid();
            console.extend(c);
            snapshot = Some(snap);
        };

        if let Some(h) = old {
            run(session, h.0, "mouseout", true);
            run(session, h.0, "mouseleave", false);
        }
        if let Some(n) = node {
            run(session, n.0, "mouseover", true);
            run(session, n.0, "mouseenter", false);
            run(session, n.0, "mousemove", true);
        }

        self.hovered_node = node;
        // The hovered node changed: invalidate layout so `:hover` rules re-cascade/repaint even
        // when no JS snapshot was produced.
        self.layout_cache = None;
        if let Some(snap) = snapshot {
            if let LoadState::Loaded {
                doc, console: c, ..
            } = &mut self.state
            {
                *doc = Some(snap);
                c.extend(console);
                return true;
            }
        }
        // Hovered node changed but produced no snapshot (e.g. both None paths): still a change.
        true
    }

    /// Deliver a physical key press to the focused text field, if any. Routes through the live JS
    /// session (fires keydown → value mutation + input → keyup), adopts the updated DOM snapshot,
    /// and invalidates the layout cache. Returns `true` if a focused field consumed the key (a
    /// re-render is warranted), `false` if there was no focused field or no session.
    pub fn dispatch_key(&mut self, key: &str, code: &str) -> bool {
        let node = match self.focused_node {
            Some(n) => n,
            None => return false,
        };
        let session = match &self.session {
            Some(s) => s,
            None => return false,
        };
        let (mut snapshot, mut console) = session.dispatch_key(node.0, key, code);
        snapshot.prune_invalid();

        // Enter in a single-line <input> (not <textarea>) inside a <form> fires `submit` on the
        // nearest ancestor form (no navigation — handlers can preventDefault as usual).
        if key == "Enter" && node.0 < snapshot.len() && is_single_line_input(&snapshot, node) {
            if let Some(form) = ancestor_form(&snapshot, node) {
                let session = self.session.as_ref().unwrap();
                let (s, c) = session.fire_event(form.0, "submit");
                snapshot = s;
                snapshot.prune_invalid();
                console.extend(c);
            }
        }

        if let LoadState::Loaded {
            doc, console: c, ..
        } = &mut self.state
        {
            *doc = Some(snapshot);
            c.extend(console);
            self.layout_cache = None;
            true
        } else {
            false
        }
    }

    /// Whether the engine currently has an editable text field (text-like `<input>` / `<textarea>`)
    /// focused in the live document. The platform layer can use this to decide whether to forward
    /// key events to the page (vs. treating them as browser shortcuts).
    pub fn has_text_focus(&self) -> bool {
        let node = match self.focused_node {
            Some(n) => n,
            None => return false,
        };
        match &self.state {
            LoadState::Loaded { doc: Some(d), .. } => {
                node.0 < d.len() && is_editable_text_field(d, node)
            }
            _ => false,
        }
    }

    /// Run any due timers / microtasks in the live page JS (e.g. deferred work, animation steps)
    /// and adopt the updated DOM snapshot. Returns `true` if a re-render is warranted.
    pub fn tick(&mut self) -> bool {
        // Make sure the JS side has up-to-date element geometry BEFORE this tick runs page script.
        // Scripts execute inside `session.tick()` and may synchronously read layout-dependent APIs
        // (`getBoundingClientRect`, `elementFromPoint`, `caretPositionFromPoint`, `caretRangeFromPoint`,
        // …) which are served from the engine-pushed `layout_rects`. Headless drivers (e.g. the WPT
        // runner) never call `render()`, so without this the rect table would stay empty and those
        // reads return 0/null. Cheap when the cache is already valid (`ensure_layout` → false, no push).
        if self.session.is_some() {
            let dw = ((self.vp_w as f32) * self.scale).round().max(1.0) as u32;
            let dh = ((self.vp_h as f32) * self.scale).round().max(1.0) as u32;
            if self.ensure_layout(dw, dh, 0.0) {
                self.push_layout_rects();
            }
        }
        let session = match &self.session {
            Some(s) => s,
            None => return false,
        };
        // `None` => nothing was due this tick; cheap no-op (no snapshot clone, no re-render).
        let mut dirty = false;
        if let Some((mut snapshot, console)) = session.tick() {
            snapshot.prune_invalid();
            if let LoadState::Loaded {
                doc, console: c, ..
            } = &mut self.state
            {
                *doc = Some(snapshot);
                c.extend(console);
                self.layout_cache = None;
                dirty = true;
            }
        }
        // Re-evaluate IntersectionObserver/ResizeObserver geometry against the current scroll
        // offset + viewport every tick. As the user scrolls, scroll_y changes and previously
        // off-screen targets become intersecting → lazy-load / reveal callbacks fire. Cheap when
        // the page has no such observers (one tiny eval).
        if self.deliver_observations() {
            dirty = true;
        }
        if self.apply_pending_scroll() {
            dirty = true;
        }
        dirty
    }

    /// Apply a JS-requested scroll (`window.scrollTo` / `element.scrollIntoView`): the JS native
    /// stored a document-CSS-px target; convert to device px and move the scroll offset. Returns
    /// whether the offset changed (so the caller re-renders). The render clamps to the page height.
    fn apply_pending_scroll(&mut self) -> bool {
        if let Some(y_css) = js::take_pending_scroll() {
            let y = (y_css * self.scale).max(0.0);
            if (y - self.scroll_y).abs() > 0.5 {
                self.scroll_y = y;
                return true;
            }
        }
        false
    }

    /// Compute IntersectionObserver / ResizeObserver geometry for the page's observed targets and,
    /// when an observation changes (or it's the first one), fire the JS callbacks.
    ///
    /// Geometry is computed in Rust from the cached layout tree (all in device pixels — layout is
    /// built at the device viewport size, so "CSS px" and device px coincide here and scroll_y is
    /// already device px). The IntersectionObserver root is the viewport. ResizeObserver reports
    /// the border-box size (we don't subtract padding/border — a documented simplification).
    ///
    /// Returns `true` if a callback fired and the DOM snapshot was adopted (so a re-render is
    /// warranted). Cheap no-op when the page has no IO/RO observers (one tiny eval per call).
    pub fn deliver_observations(&mut self) -> bool {
        // Read the observed-targets list (empty when the page registered no IO/RO observers).
        let targets_json = match &self.session {
            Some(s) => s.observed_targets(),
            None => return false,
        };
        if targets_json.is_empty() || targets_json == "[]" {
            return false;
        }
        let targets: Vec<ObservedTarget> = match parse_observed_targets(&targets_json) {
            Some(t) if !t.is_empty() => t,
            _ => return false,
        };

        // Make sure layout reflects the current viewport, then map NodeId -> border-box rect.
        let dw = ((self.vp_w as f32) * self.scale).round().max(1.0) as u32;
        let dh = ((self.vp_h as f32) * self.scale).round().max(1.0) as u32;
        self.ensure_layout(dw, dh, 0.0);
        let cache = match &self.layout_cache {
            Some(c) => c,
            None => return false,
        };
        let mut rects: HashMap<usize, layout::Rect> = HashMap::new();
        collect_node_rects(&cache.root, &mut rects);

        // Viewport visible region in layout (device-px) coords.
        let root_w = dw as f32;
        let root_h = dh as f32;
        let view_top = self.scroll_y;
        let view_bottom = self.scroll_y + root_h;
        let view_left = 0.0_f32;
        let view_right = root_w;

        // Build the delivery JSON, recording only changed/initial observations.
        let mut items: Vec<String> = Vec::new();
        for t in &targets {
            let rect = match rects.get(&t.node_id) {
                Some(r) => *r,
                None => continue, // not laid out (display:none / detached): no geometry to report
            };
            match t.kind {
                ObsKind::Io => {
                    // Element rect in document coords; intersection with the viewport region.
                    let ex0 = rect.x;
                    let ey0 = rect.y;
                    let ex1 = rect.x + rect.width;
                    let ey1 = rect.y + rect.height;
                    let ix0 = ex0.max(view_left);
                    let iy0 = ey0.max(view_top);
                    let ix1 = ex1.min(view_right);
                    let iy1 = ey1.min(view_bottom);
                    let iw = (ix1 - ix0).max(0.0);
                    let ih = (iy1 - iy0).max(0.0);
                    let overlap = iw * ih;
                    let elem_area = (rect.width * rect.height).max(1.0);
                    let is_intersecting = overlap > 0.0;
                    let ratio = (overlap / elem_area).clamp(0.0, 1.0);
                    let key = (t.observer_id, t.node_id);
                    let changed = match self.prev_intersecting.get(&key) {
                        Some(&prev) => prev != is_intersecting,
                        None => true, // initial observation always fires
                    };
                    self.prev_intersecting.insert(key, is_intersecting);
                    if !changed {
                        continue;
                    }
                    // Report the element rect relative to the viewport (clientRect-style: subtract
                    // the scroll offset) so JS sees usual top/left semantics.
                    items.push(format!(
                        "{{\"kind\":\"io\",\"observerId\":{},\"nodeId\":{},\"isIntersecting\":{},\"intersectionRatio\":{},\"x\":{},\"y\":{},\"width\":{},\"height\":{},\"ix\":{},\"iy\":{},\"iw\":{},\"ih\":{},\"rootW\":{},\"rootH\":{}}}",
                        t.observer_id, t.node_id, is_intersecting, ratio,
                        fnum(rect.x - view_left), fnum(rect.y - view_top), fnum(rect.width), fnum(rect.height),
                        fnum(ix0 - view_left), fnum(iy0 - view_top), fnum(iw), fnum(ih),
                        fnum(root_w), fnum(root_h),
                    ));
                }
                ObsKind::Ro => {
                    let w = rect.width;
                    let h = rect.height;
                    let key = (t.observer_id, t.node_id);
                    let changed = match self.prev_size.get(&key) {
                        Some(&(pw, ph)) => (pw - w).abs() > 0.01 || (ph - h).abs() > 0.01,
                        None => true, // initial observation always fires
                    };
                    self.prev_size.insert(key, (w, h));
                    if !changed {
                        continue;
                    }
                    items.push(format!(
                        "{{\"kind\":\"ro\",\"observerId\":{},\"nodeId\":{},\"x\":{},\"y\":{},\"width\":{},\"height\":{}}}",
                        t.observer_id, t.node_id, fnum(0.0), fnum(0.0), fnum(w), fnum(h),
                    ));
                }
            }
        }

        if items.is_empty() {
            return false;
        }
        let arr = format!("[{}]", items.join(","));
        let session = match &self.session {
            Some(s) => s,
            None => return false,
        };
        let (mut snapshot, console) = session.deliver_observations(&arr);
        snapshot.prune_invalid();
        if let LoadState::Loaded {
            doc, console: c, ..
        } = &mut self.state
        {
            *doc = Some(snapshot);
            c.extend(console);
            self.layout_cache = None; // callbacks may have mutated the DOM
            true
        } else {
            false
        }
    }

    /// Visible text of the currently-loaded document (empty if none). Handy for tests/diagnostics.
    pub fn visible_text(&self) -> String {
        match &self.state {
            LoadState::Loaded { doc: Some(d), .. } => extract_visible_text(d),
            _ => String::new(),
        }
    }

    /// Console + error lines captured for the current page (diagnostics / devtools Console tab).
    pub fn console_lines(&self) -> Vec<String> {
        match &self.state {
            LoadState::Loaded { console, .. } => console.clone(),
            _ => Vec::new(),
        }
    }

    /// Devtools console REPL: evaluate `code` in the live page's JS context, adopt any DOM
    /// changes, and return the result (or error) as a display string. No-op if no live session.
    pub fn console_eval(&mut self, code: &str) -> String {
        let session = match &self.session {
            Some(s) => s,
            None => return "(no live page)".to_string(),
        };
        let (display, mut snapshot, console) = session.repl_eval(code);
        snapshot.prune_invalid();
        if let LoadState::Loaded {
            doc, console: c, ..
        } = &mut self.state
        {
            *doc = Some(snapshot);
            c.extend(console);
            self.layout_cache = None; // the eval may have mutated the DOM
        }
        display
    }

    /// Network activity for the current navigation, as a JSON array (for the devtools Network tab):
    /// `[{"method","url","status","ok","ms","size","type"}, ...]`.
    pub fn network_log_json(&self) -> String {
        let mut s = String::from("[");
        for (i, e) in net::network_log().iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"method\":{},\"url\":{},\"status\":{},\"ok\":{},\"ms\":{},\"size\":{},\"type\":{}}}",
                json_str(&e.method),
                json_str(&e.url),
                e.status,
                e.ok,
                e.duration_ms,
                e.size,
                json_str(&e.content_type),
            ));
        }
        s.push(']');
        s
    }

    /// Serialize the current document's tree as nested JSON for the DevTools "Elements" tab. Each
    /// node is `{"id":N,"type":"element"|"text","tag":..,"attrs":{..},"text":..,"children":[..]}`.
    /// Text nodes carry the whitespace-collapsed/trimmed string and no children; empty/all-whitespace
    /// text nodes are skipped. Elements carry `tag`, all `attrs`, and their (recursively serialized)
    /// children. The serialized root is the document root's element subtree (`<html>`). Returns `"{}"`
    /// when there is no document. Depth is capped (`MAX_DOM_DEPTH`) to guard pathological nesting.
    pub fn dom_tree_json(&self) -> String {
        let doc = match &self.state {
            LoadState::Loaded { doc: Some(d), .. } => d,
            _ => return "{}".to_string(),
        };
        // Find the root element to start at (the first <html> element child, else the first element
        // child of the document root, else the document root itself).
        let root = doc.root();
        let start = doc
            .get(root)
            .children
            .iter()
            .copied()
            .find(|&c| matches!(&doc.get(c).data, dom::NodeData::Element(_)))
            .unwrap_or(root);
        let mut out = String::new();
        if !serialize_dom_node(doc, start, 0, &mut out) {
            // Start node was a skipped/empty text node (unlikely for the root); emit empty object.
            return "{}".to_string();
        }
        out
    }

    /// The element NodeId under DEVICE-pixel point `(x, y)` (viewport-relative): hit-test the cached
    /// layout in document space (`y + scroll_y`), then walk up to the nearest element. Used for the
    /// right-click "Inspect Element" flow. `None` if there's no layout/DOM or no element is hit.
    pub fn node_at_point(&self, x: f32, y: f32) -> Option<usize> {
        let cache = self.layout_cache.as_ref()?;
        let doc = match &self.state {
            LoadState::Loaded { doc: Some(d), .. } => d,
            _ => return None,
        };
        let node = deepest_node_at(&cache.root, x, y + self.scroll_y)?;
        // Walk up to the nearest ancestor-or-self that is an element.
        let mut cur = Some(node);
        while let Some(id) = cur {
            if id.0 < doc.len() {
                if let dom::NodeData::Element(_) = &doc.get(id).data {
                    return Some(id.0);
                }
            }
            cur = doc.get(id).parent;
        }
        None
    }

    /// Set (or clear, with `None`) the DevTools Elements highlight node. The next `render` draws a
    /// translucent overlay over that node's laid-out border box. An out-of-range id is ignored.
    pub fn set_inspect_node(&mut self, node: Option<usize>) {
        self.inspect_node = match node {
            Some(id) => {
                let valid =
                    matches!(&self.state, LoadState::Loaded { doc: Some(d), .. } if id < d.len());
                if valid {
                    Some(dom::NodeId(id))
                } else {
                    None
                }
            }
            None => None,
        };
    }

    /// Test-only: focus the first editable text field in the live document (by walking the DOM),
    /// returning whether one was found. Sidesteps coordinate-precise click-to-focus in tests.
    #[cfg(test)]
    fn focus_first_text_field(&mut self) -> bool {
        let found = match &self.state {
            LoadState::Loaded { doc: Some(d), .. } => {
                fn walk(doc: &dom::Document, id: dom::NodeId) -> Option<dom::NodeId> {
                    if is_editable_text_field(doc, id) {
                        return Some(id);
                    }
                    for &c in &doc.get(id).children {
                        if let Some(f) = walk(doc, c) {
                            return Some(f);
                        }
                    }
                    None
                }
                walk(d, d.root())
            }
            _ => None,
        };
        self.focused_node = found;
        found.is_some()
    }

    /// Test-only: the value of attribute `name` on the live document's `<body>`.
    #[cfg(test)]
    fn body_attr(&self, name: &str) -> Option<String> {
        match &self.state {
            LoadState::Loaded { doc: Some(d), .. } => {
                let body = find_tag(d, d.root(), "body")?;
                match &d.get(body).data {
                    dom::NodeData::Element(e) => e.attrs.get(name).cloned(),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Test-only: the `value` attribute of a node in the live document.
    #[cfg(test)]
    fn node_attr(&self, id: dom::NodeId, name: &str) -> Option<String> {
        match &self.state {
            LoadState::Loaded { doc: Some(d), .. } => match &d.get(id).data {
                dom::NodeData::Element(e) => e.attrs.get(name).cloned(),
                _ => None,
            },
            _ => None,
        }
    }

    /// Test-only: the node id of the currently focused field.
    #[cfg(test)]
    fn focused_node_for_test(&self) -> Option<dom::NodeId> {
        self.focused_node
    }

    /// Test-only: number of decoded `<img>` images for the current page.
    #[cfg(test)]
    fn decoded_image_count(&self) -> usize {
        match &self.state {
            LoadState::Loaded { images, .. } => images.len(),
            _ => 0,
        }
    }

    /// Test-only: the (w, h) of the first decoded image, if any.
    #[cfg(test)]
    fn first_decoded_image_size(&self) -> Option<(u32, u32)> {
        match &self.state {
            LoadState::Loaded { images, .. } => images.values().next().map(|i| (i.w, i.h)),
            _ => None,
        }
    }

    /// Test-only: device-pixel center of the cached layout box for `id` (inverse of the
    /// layout→device mapping in `render`: left=0, header_h=0, so device = layout - scroll_y).
    #[cfg(test)]
    fn node_center_device(&self, id: dom::NodeId) -> Option<(f32, f32)> {
        fn find(b: &layout::LayoutBox, id: dom::NodeId) -> Option<&layout::LayoutBox> {
            // Prefer the element's own (non-text) box; recurse depth-first.
            for c in &b.children {
                if let Some(f) = find(c, id) {
                    return Some(f);
                }
            }
            if b.node == Some(id) {
                Some(b)
            } else {
                None
            }
        }
        let cache = self.layout_cache.as_ref()?;
        let bx = find(&cache.root, id)?;
        let r = bx.dimensions.border_box();
        let lx = r.x + r.width / 2.0;
        let ly = r.y + r.height / 2.0;
        Some((lx, ly - self.scroll_y))
    }

    /// Test-only: the device-px border-box rect of a node (scroll-folded, viewport-relative).
    #[cfg(test)]
    fn node_device_rect(&self, id: dom::NodeId) -> Option<layout::Rect> {
        fn find(b: &layout::LayoutBox, id: dom::NodeId) -> Option<&layout::LayoutBox> {
            for c in &b.children {
                if let Some(f) = find(c, id) {
                    return Some(f);
                }
            }
            if b.node == Some(id) {
                Some(b)
            } else {
                None
            }
        }
        let cache = self.layout_cache.as_ref()?;
        let bx = find(&cache.root, id)?;
        let mut r = bx.dimensions.border_box();
        r.y -= self.scroll_y;
        Some(r)
    }

    /// Test-only: walk the live DOM for the first element with the given `id` attribute.
    #[cfg(test)]
    fn node_by_attr_id(&self, attr_id: &str) -> Option<dom::NodeId> {
        match &self.state {
            LoadState::Loaded { doc: Some(d), .. } => find_by_attr_id(d, d.root(), attr_id),
            _ => None,
        }
    }

    /// Test-only: the value of attribute `name` on the live document's `<body>`.
    #[cfg(test)]
    fn visible_attr_body(&self, name: &str) -> Option<String> {
        let d = match &self.state {
            LoadState::Loaded { doc: Some(d), .. } => d,
            _ => return None,
        };
        fn find_body(doc: &dom::Document, id: dom::NodeId) -> Option<dom::NodeId> {
            if let dom::NodeData::Element(e) = &doc.get(id).data {
                if e.tag.eq_ignore_ascii_case("body") {
                    return Some(id);
                }
            }
            for &c in &doc.get(id).children {
                if let Some(f) = find_body(doc, c) {
                    return Some(f);
                }
            }
            None
        }
        let body = find_body(d, d.root())?;
        match &d.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get(name).cloned(),
            _ => None,
        }
    }
}

/// A simple computed vertical gradient — proof the pixels came from our code.
/// The page canvas background. CSS "background propagation": the canvas takes the root element's
/// (`<html>`) background; if that's transparent, the `<body>`'s background propagates up. Defaults
/// to white when neither sets one. Walks the first-child chain (html → body) so it never picks up a
/// content element's background.
fn page_background(root: &layout::LayoutBox, root_scheme_dark: bool) -> Color {
    let mut node = root;
    for _ in 0..3 {
        if let Some((r, g, b)) = node.style.background_color {
            return Color::rgb(r, g, b);
        }
        match node.children.first() {
            Some(c) => node = c,
            None => break,
        }
    }
    // No explicit html/body background: default canvas is white, or dark (`#1e1e1e`) when the page
    // opted into a dark `color-scheme` (resolved during the cascade and stored on the layout cache).
    if root_scheme_dark {
        Color::rgb(0x1e, 0x1e, 0x1e)
    } else {
        Color::WHITE
    }
}

/// Draw a macOS-style overlay scrollbar (a semi-transparent rounded thumb on the right edge) when
/// the document is taller than the viewport. `top` is the content area's top (device px), `viewport_h`
/// its height, `content_h` the full document height, `scroll_y` the current offset — all device px.
fn paint_scrollbar(
    fb: &mut Framebuffer,
    top: f32,
    viewport_h: f32,
    content_h: f32,
    scroll_y: f32,
    scale: f32,
) {
    if content_h <= viewport_h + 1.0 || viewport_h <= 1.0 {
        return; // nothing to scroll
    }
    let w = (7.0 * scale).round().max(4.0);
    let margin = 2.0 * scale;
    let x = (fb.width as f32 - w - margin).round() as i32;
    let min_thumb = 28.0 * scale;
    let thumb_h = ((viewport_h / content_h) * viewport_h).clamp(min_thumb, viewport_h);
    let max_off = (viewport_h - thumb_h).max(0.0);
    let frac = (scroll_y / (content_h - viewport_h)).clamp(0.0, 1.0);
    let y = (top + frac * max_off).round() as i32;
    // Semi-transparent neutral grey reads on both light and dark pages.
    let thumb = Color {
        r: 128,
        g: 128,
        b: 128,
        a: 150,
    };
    fb.fill_round_rect(
        Rect {
            x,
            y,
            w: w.round() as i32,
            h: thumb_h.round() as i32,
        },
        w / 2.0,
        thumb,
    );
}

fn paint_gradient(fb: &mut Framebuffer) {
    let h = fb.height.max(1);
    for y in 0..fb.height {
        let t = y as f32 / h as f32;
        let c = Color::rgb(
            (18.0 + t * 10.0) as u8,
            (20.0 + t * 14.0) as u8,
            (28.0 + t * 26.0) as u8,
        );
        fb.fill_rect(
            Rect {
                x: 0,
                y: y as i32,
                w: fb.width as i32,
                h: 1,
            },
            c,
        );
    }
}

/// Draw a left-anchored string with its baseline at `baseline_y`. Returns the final pen x.
fn draw_text(
    fb: &mut Framebuffer,
    font: &dyn GlyphRasterizer,
    text: &str,
    x: f32,
    baseline_y: f32,
    px: f32,
    color: Color,
) -> f32 {
    draw_text_spaced(fb, font, text, x, baseline_y, px, color, 0.0)
}

/// Like [`draw_text`] but adds `letter_spacing` px to the pen after each character. Returns the
/// final pen x (after the last glyph's advance + spacing), used to size text-decoration lines.
#[allow(clippy::too_many_arguments)]
fn draw_text_spaced(
    fb: &mut Framebuffer,
    font: &dyn GlyphRasterizer,
    text: &str,
    x: f32,
    baseline_y: f32,
    px: f32,
    color: Color,
    letter_spacing: f32,
) -> f32 {
    let mut pen = x;
    for ch in text.chars() {
        if let Some(g) = font.rasterize(ch, px) {
            for row in 0..g.height {
                for col in 0..g.width {
                    let cov = g.coverage[row * g.width + col];
                    if cov == 0 {
                        continue;
                    }
                    let dx = pen as i32 + g.left + col as i32;
                    let dy = baseline_y as i32 + g.top + row as i32;
                    fb.blend_coverage(dx, dy, cov, color);
                }
            }
            pen += g.advance;
        } else {
            pen += font.advance(ch, px);
        }
        pen += letter_spacing;
    }
    pen
}

/// A [`layout::TextMeasurer`] backed by our [`SystemFont`], so layout can size text without
/// knowing about font rasterization. Widths mirror what the painter actually draws.
struct FontMeasurer<'a> {
    font: &'a SystemFont,
}

impl layout::TextMeasurer for FontMeasurer<'_> {
    fn text_width(&self, text: &str, px: f32, bold: bool) -> f32 {
        let mut w: f32 = text.chars().map(|ch| self.font.advance(ch, px)).sum();
        if bold {
            // Faux-bold draws each glyph twice with a 1px offset, widening the run by ~1px/glyph.
            w += text.chars().count() as f32;
        }
        w
    }

    fn line_height(&self, px: f32) -> f32 {
        px * 1.3
    }
}

/// Hit-test a layout subtree at layout coordinates `(x, y)`, returning the DOM node of the
/// deepest box whose border box contains the point and that carries a `node`. Children are
/// searched first (and in order) so the deepest / topmost box wins; a box's own border box is
/// its hit area.
fn deepest_node_at(b: &layout::LayoutBox, x: f32, y: f32) -> Option<dom::NodeId> {
    // Recurse into children first so a deeper hit takes precedence over this box.
    for c in &b.children {
        if let Some(n) = deepest_node_at(c, x, y) {
            return Some(n);
        }
    }
    let r = b.dimensions.border_box();
    let inside = x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
    if inside {
        b.node
    } else {
        None
    }
}

/// Collect the descendant `<option>` ids of a `<select>` depth-first (including those nested in
/// `<optgroup>`). Mirrors the layout crate's `selected_option_text` walk so the option order /
/// indices agree between what we render and what the dropdown menu offers.
fn collect_options(doc: &dom::Document, select_id: dom::NodeId) -> Vec<dom::NodeId> {
    let mut out = Vec::new();
    fn walk(doc: &dom::Document, id: dom::NodeId, out: &mut Vec<dom::NodeId>) {
        for &child in &doc.get(id).children {
            if child.0 >= doc.len() {
                continue;
            }
            if let dom::NodeData::Element(el) = &doc.get(child).data {
                if el.tag.eq_ignore_ascii_case("option") {
                    out.push(child);
                }
            }
            walk(doc, child, out);
        }
    }
    walk(doc, select_id, &mut out);
    out
}

/// Collapsed text content of an `<option>` (its descendant text nodes, whitespace-collapsed) — the
/// label shown for that option in the dropdown menu.
fn option_text(doc: &dom::Document, opt: dom::NodeId) -> String {
    fn gather(doc: &dom::Document, id: dom::NodeId, s: &mut String) {
        for &child in &doc.get(id).children {
            if child.0 >= doc.len() {
                continue;
            }
            match &doc.get(child).data {
                dom::NodeData::Text(t) => s.push_str(t),
                dom::NodeData::Element(_) => gather(doc, child, s),
                _ => {}
            }
        }
    }
    let mut s = String::new();
    gather(doc, opt, &mut s);
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The 0-based index (into `options`) of the currently-selected `<option>`, using the same priority
/// as the layout crate's `selected_option_text`: an `<option selected>`, else the option whose
/// value matches the select's `value` attr, else the first option (index 0).
fn selected_option_index(
    doc: &dom::Document,
    select_id: dom::NodeId,
    options: &[dom::NodeId],
) -> usize {
    // 1. An <option selected>.
    for (i, &opt) in options.iter().enumerate() {
        if let dom::NodeData::Element(el) = &doc.get(opt).data {
            if el.attrs.contains_key("selected") {
                return i;
            }
        }
    }
    // 2. The option whose value matches the select's `value`.
    if let dom::NodeData::Element(sel) = &doc.get(select_id).data {
        if let Some(want) = sel.attrs.get("value") {
            for (i, &opt) in options.iter().enumerate() {
                if let dom::NodeData::Element(el) = &doc.get(opt).data {
                    let val = match el.attrs.get("value") {
                        Some(v) => v.clone(),
                        None => option_text(doc, opt),
                    };
                    if &val == want {
                        return i;
                    }
                }
            }
        }
    }
    // 3. The first option.
    0
}

/// Kind of observer a target belongs to.
#[derive(Clone, Copy, PartialEq)]
enum ObsKind {
    Io,
    Ro,
}

/// One observed IntersectionObserver/ResizeObserver target (parsed from `__observedTargets()`).
struct ObservedTarget {
    kind: ObsKind,
    observer_id: u64,
    node_id: usize,
}

/// Parse the `[{kind,observerId,nodeId}, ...]` JSON produced by `__observedTargets()`. Hand-rolled
/// (no serde dep): scans for the three fields per object. Returns `None` only on a malformed list.
fn parse_observed_targets(json: &str) -> Option<Vec<ObservedTarget>> {
    let mut out = Vec::new();
    let bytes = json.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        let end = json[i..].find('}').map(|e| i + e)?;
        let obj = &json[i..=end];
        let kind = if obj.contains("\"kind\":\"io\"") {
            ObsKind::Io
        } else if obj.contains("\"kind\":\"ro\"") {
            ObsKind::Ro
        } else {
            i = end + 1;
            continue;
        };
        let observer_id = json_number_field(obj, "observerId")? as u64;
        let node_id = json_number_field(obj, "nodeId")? as usize;
        out.push(ObservedTarget {
            kind,
            observer_id,
            node_id,
        });
        i = end + 1;
    }
    Some(out)
}

/// Extract the integer value of `"field":N` from a small JSON object slice.
fn json_number_field(obj: &str, field: &str) -> Option<f64> {
    let needle = format!("\"{field}\":");
    let start = obj.find(&needle)? + needle.len();
    let rest = &obj[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '-' || c == '.'))
        .unwrap_or(rest.len());
    rest[..end].parse::<f64>().ok()
}

/// Map every laid-out DOM node to its border-box rect (device px). When a node appears as multiple
/// boxes, the first (outermost in document order) wins — that's the element's principal box.
/// One laid-out text run: its absolute (document-space) content rect and the (already
/// whitespace-collapsed / transformed) string the painter draws. The font size used to measure /
/// paint it is carried so advance accumulation matches exactly.
#[derive(Debug, Clone)]
struct TextRun {
    rect: layout::Rect,
    text: String,
    font_size: f32,
    letter_spacing: f32,
}

/// Walk the layout tree depth-first, collecting every `Text` run in reading (paint) order. Each
/// run carries its absolute content rect (document space). This is the ordered list selection
/// resolution and highlight painting both index into.
fn collect_text_runs(root: &layout::LayoutBox) -> Vec<TextRun> {
    let mut out = Vec::new();
    fn walk(b: &layout::LayoutBox, out: &mut Vec<TextRun>) {
        if let layout::BoxContent::Text(s) = &b.content {
            if !s.is_empty() {
                out.push(TextRun {
                    rect: b.dimensions.content,
                    text: s.clone(),
                    font_size: b.style.font_size,
                    letter_spacing: b.style.letter_spacing,
                });
            }
        }
        for c in &b.children {
            walk(c, out);
        }
    }
    walk(root, &mut out);
    out
}

/// The character index within `run` nearest to document x-coordinate `x`: accumulate per-glyph
/// advances (the same `font.advance` + `letter_spacing` the painter uses) from the run's left edge
/// until the pen passes the midpoint of the next glyph, clamped to `[0, char_count]`.
fn char_index_in_run(run: &TextRun, font: &SystemFont, x: f32) -> usize {
    let px = run.font_size;
    let mut pen = run.rect.x;
    for (i, ch) in run.text.chars().enumerate() {
        let adv = font.advance(ch, px) + run.letter_spacing;
        // Click lands in this glyph's first half -> caret before it; second half -> after it.
        if x < pen + adv * 0.5 {
            return i;
        }
        pen += adv;
    }
    run.text.chars().count()
}

/// Resolve a DOCUMENT-space point to a text position `(run_index, char_index)` — a global linear
/// order (run first, then char). Pick the run whose vertical band (its content rect, extended a
/// little for inter-line slack) contains `p.y`; among candidate runs on that line, the one whose
/// horizontal span contains `p.x`, else the nearest. Falls back to the closest run by vertical
/// distance when the point is above/below all text.
fn resolve_text_position(runs: &[TextRun], font: &SystemFont, p: Point) -> (usize, usize) {
    if runs.is_empty() {
        return (0, 0);
    }
    // Candidate runs whose vertical extent contains p.y (the "line" the point is on).
    let mut best_on_line: Option<usize> = None;
    let mut best_dx = f32::MAX;
    for (i, r) in runs.iter().enumerate() {
        let top = r.rect.y;
        let bottom = r.rect.y + r.rect.height;
        if p.y >= top && p.y < bottom {
            // Horizontal distance from the point to this run's span (0 if inside).
            let left = r.rect.x;
            let right = r.rect.x + r.rect.width;
            let dx = if p.x < left {
                left - p.x
            } else if p.x > right {
                p.x - right
            } else {
                0.0
            };
            if dx < best_dx {
                best_dx = dx;
                best_on_line = Some(i);
            }
        }
    }
    if let Some(i) = best_on_line {
        return (i, char_index_in_run(&runs[i], font, p.x));
    }

    // Point is on no run's line: choose the run with the smallest vertical distance, tie-broken by
    // horizontal distance, so dragging into the margin above/below still selects sensibly.
    let mut best = 0usize;
    let mut best_metric = f32::MAX;
    for (i, r) in runs.iter().enumerate() {
        let cy = r.rect.y + r.rect.height * 0.5;
        let dy = (p.y - cy).abs();
        let left = r.rect.x;
        let right = r.rect.x + r.rect.width;
        let dx = if p.x < left {
            left - p.x
        } else if p.x > right {
            p.x - right
        } else {
            0.0
        };
        let metric = dy * 1000.0 + dx; // vertical dominates; horizontal breaks ties
        if metric < best_metric {
            best_metric = metric;
            best = i;
        }
    }
    (best, char_index_in_run(&runs[best], font, p.x))
}

/// Maximum DOM depth serialized by [`Engine::dom_tree_json`]; deeper subtrees are truncated (their
/// children omitted) to guard against pathologically nested documents.
const MAX_DOM_DEPTH: usize = 512;

/// Serialize a single DOM node (and its subtree) into `out` as the JSON object documented on
/// [`Engine::dom_tree_json`]. Returns `false` (and writes nothing) when the node is an empty /
/// all-whitespace text node (or a non-rendered node kind), so callers can skip it.
fn serialize_dom_node(
    doc: &dom::Document,
    id: dom::NodeId,
    depth: usize,
    out: &mut String,
) -> bool {
    if id.0 >= doc.len() {
        return false;
    }
    match &doc.get(id).data {
        dom::NodeData::Text(t) => {
            let collapsed = t.split_whitespace().collect::<Vec<_>>().join(" ");
            if collapsed.is_empty() {
                return false;
            }
            out.push_str(&format!("{{\"id\":{},\"type\":\"text\",\"text\":", id.0));
            out.push_str(&json_str(&collapsed));
            out.push('}');
            true
        }
        dom::NodeData::Element(el) => {
            out.push_str(&format!("{{\"id\":{},\"type\":\"element\",\"tag\":", id.0));
            out.push_str(&json_str(&el.tag));
            out.push_str(",\"attrs\":{");
            // Deterministic attribute order (HashMap iteration is unordered).
            let mut keys: Vec<&String> = el.attrs.keys().collect();
            keys.sort();
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&json_str(k));
                out.push(':');
                out.push_str(&json_str(&el.attrs[*k]));
            }
            out.push_str("},\"children\":[");
            if depth < MAX_DOM_DEPTH {
                let mut first = true;
                for &child in &doc.get(id).children {
                    let mut child_out = String::new();
                    if serialize_dom_node(doc, child, depth + 1, &mut child_out) {
                        if !first {
                            out.push(',');
                        }
                        out.push_str(&child_out);
                        first = false;
                    }
                }
            }
            out.push_str("]}");
            true
        }
        // Document / Comment nodes aren't part of the rendered element tree we expose.
        _ => false,
    }
}

fn collect_node_rects(b: &layout::LayoutBox, out: &mut HashMap<usize, layout::Rect>) {
    if let Some(node) = b.node {
        out.entry(node.0)
            .or_insert_with(|| b.dimensions.border_box());
    }
    for c in &b.children {
        collect_node_rects(c, out);
    }
}

/// Collect each positioned box's CSSOM *used* inset values `[top, right, bottom, left]` (device px,
/// as stored by layout). Pushed to the JS Session so `getComputedStyle(el).top` etc. report the used
/// value when the element has a box (`crates/layout` fills `used_insets` during positioned layout).
fn collect_used_insets(b: &layout::LayoutBox, out: &mut HashMap<usize, [f32; 4]>) {
    if let (Some(node), Some(insets)) = (b.node, b.used_insets) {
        out.entry(node.0).or_insert(insets);
    }
    for c in &b.children {
        collect_used_insets(c, out);
    }
}

/// Like `collect_used_insets`, but for each box's resolved margins `[top, right, bottom, left]`
/// (device px). Pushed to the JS Session so `getComputedStyle(el).margin*` reports the used value
/// (e.g. an `auto` margin resolved to a centering offset).
fn collect_used_margins(b: &layout::LayoutBox, out: &mut HashMap<usize, [f32; 4]>) {
    if let (Some(node), Some(margins)) = (b.node, b.used_margins) {
        out.entry(node.0).or_insert(margins);
    }
    for c in &b.children {
        collect_used_margins(c, out);
    }
}

/// Standalone layout pass that produces the per-node rect table (CSS px, document-absolute) for a
/// `doc` + `styles`, WITHOUT touching `Engine` state. Used to seed the JS session's `layout_rects`
/// BEFORE its scripts run, so synchronous layout-dependent reads during page load
/// (`getBoundingClientRect`, `elementFromPoint`, `caretPositionFromPoint`, `caretRangeFromPoint`, …)
/// see real geometry rather than 0/null. The engine recomputes the authoritative layout after
/// scripts/external CSS load and re-pushes; this is the best-effort first frame using whatever
/// stylesheets/images are available at parse time. Returns
/// `(rects, naturals, insets, scroll_y_css, doc_height_css)` in the exact shape `set_layout_rects`
/// expects (CSS px). `None` when there's no font (then nothing is seeded).
#[allow(clippy::type_complexity)]
fn compute_initial_rects(
    doc: &dom::Document,
    styles: &[css::Stylesheet],
    images: &HashMap<dom::NodeId, DecodedImage>,
    font: Option<&SystemFont>,
    vp_w: u32,
    vp_h: u32,
    scale: f32,
    is_dark: bool,
) -> Option<(
    Vec<(usize, f32, f32, f32, f32)>,
    Vec<(usize, f32, f32)>,
    Vec<(usize, f32, f32, f32, f32)>,
    Vec<(usize, f32, f32, f32, f32)>,
    f32,
    f32,
)> {
    let font = font?;
    let dw = ((vp_w as f32) * scale).round().max(1.0) as u32;
    let dh = ((vp_h as f32) * scale).round().max(1.0) as u32;
    style::set_viewport_metrics(vp_w as f32, vp_h as f32, scale);
    style::set_interaction_state(None, None);
    let vw = (dw as f32).max(1.0);
    let vh = (dh as f32).max(1.0);
    let measurer = FontMeasurer { font };
    let mut intrinsic_sizes: HashMap<dom::NodeId, (f32, f32)> = images
        .iter()
        .map(|(&id, img)| (id, (img.w as f32, img.h as f32)))
        .collect();
    collect_canvas_intrinsics(doc, &mut intrinsic_sizes);
    collect_svg_intrinsics(doc, &mut intrinsic_sizes);
    let (computed, _root_scheme_dark) = style::cascade_with_root_scheme(doc, styles, is_dark);
    let root = layout::layout_document(doc, &computed, vw, vh, &measurer, &intrinsic_sizes, None);
    let content_h = root.dimensions.margin_box().height;

    let mut rects: HashMap<usize, layout::Rect> = HashMap::new();
    collect_node_rects(&root, &mut rects);
    let inv = if scale > 0.0 { 1.0 / scale } else { 1.0 };
    let rect_list: Vec<(usize, f32, f32, f32, f32)> = rects
        .iter()
        .map(|(&id, r)| (id, r.x * inv, r.y * inv, r.width * inv, r.height * inv))
        .collect();
    let naturals: Vec<(usize, f32, f32)> = images
        .iter()
        .map(|(&id, img)| (id.0, img.w as f32, img.h as f32))
        .collect();
    let mut insets: HashMap<usize, [f32; 4]> = HashMap::new();
    collect_used_insets(&root, &mut insets);
    let inset_list: Vec<(usize, f32, f32, f32, f32)> = insets
        .iter()
        .map(|(&id, v)| (id, v[0] * inv, v[1] * inv, v[2] * inv, v[3] * inv))
        .collect();
    let mut margins: HashMap<usize, [f32; 4]> = HashMap::new();
    collect_used_margins(&root, &mut margins);
    let margin_list: Vec<(usize, f32, f32, f32, f32)> = margins
        .iter()
        .map(|(&id, v)| (id, v[0] * inv, v[1] * inv, v[2] * inv, v[3] * inv))
        .collect();
    Some((
        rect_list,
        naturals,
        inset_list,
        margin_list,
        0.0,
        content_h * inv,
    ))
}

/// Collect every masked box: its node id, border-box rect (device px), and the resolved
/// [`style::MaskImage`]. Used to build per-box mask coverage bitmaps. Only boxes carrying a DOM node
/// and a `mask-image` in their paint extras qualify.
fn collect_mask_targets(
    b: &layout::LayoutBox,
    out: &mut Vec<(dom::NodeId, layout::Rect, style::MaskImage)>,
) {
    if let Some(node) = b.node {
        if let Some(mask) = b.style.extras.as_deref().and_then(|e| e.mask_image.clone()) {
            out.push((node, b.dimensions.border_box(), mask));
        }
    }
    for c in &b.children {
        collect_mask_targets(c, out);
    }
}

/// Fetch + decode a `mask-image` source `url` (resolved against `base`) into a [`MaskSource`].
/// Handles `data:` URLs (percent-encoded or base64; SVG kept as text, raster decoded) and
/// same-origin/absolute http(s) urls (fetched like an `<img>`). SVG payloads are recognized by a
/// `image/svg` media type or by sniffing `<svg` markup. Returns `None` on fetch/decode failure.
fn load_mask_source(url: &str, base: &str) -> Option<MaskSource> {
    let url = url.trim();
    if let Some(rest) = url.strip_prefix("data:") {
        // Split the `data:` URL into media type + payload to decide SVG-vs-raster up front.
        let comma = rest.find(',')?;
        let meta = &rest[..comma].to_ascii_lowercase();
        let bytes = decode_data_url(url)?;
        if meta.contains("image/svg") {
            return Some(MaskSource::Svg(
                String::from_utf8_lossy(&bytes).into_owned(),
            ));
        }
        // No explicit svg type: sniff the decoded bytes for `<svg`.
        if sniff_svg(&bytes) {
            return Some(MaskSource::Svg(
                String::from_utf8_lossy(&bytes).into_owned(),
            ));
        }
        return decode_image(&bytes).map(MaskSource::Raster);
    }
    // Network / same-origin: resolve and fetch.
    let abs = resolve_url(base, url)?;
    let resp = net::fetch(&abs).ok()?;
    if abs.to_ascii_lowercase().ends_with(".svg") || sniff_svg(&resp.body) {
        return Some(MaskSource::Svg(
            String::from_utf8_lossy(&resp.body).into_owned(),
        ));
    }
    decode_image(&resp.body).map(MaskSource::Raster)
}

/// Cheap heuristic: does this byte slice look like SVG markup? (Looks for `<svg` near the start,
/// skipping a leading XML declaration / whitespace.)
fn sniff_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(512)];
    let s = String::from_utf8_lossy(head).to_ascii_lowercase();
    s.contains("<svg")
}

/// Rasterize a [`MaskSource`] to an `out_w × out_h` coverage bitmap (RGBA, but only the ALPHA
/// channel is meaningful — it's the mask coverage). `size` picks the fit: `Stretch` fills the box
/// exactly; `Contain`/`Cover` preserve aspect ratio (the engine's SVG rasterizer already does a
/// contain-style `xMidYMid meet` fit, so for SVG we rasterize straight at the box size; for raster
/// masks we fit the source rect and centre it).
fn rasterize_mask_coverage(
    src: &MaskSource,
    out_w: u32,
    out_h: u32,
    size: style::MaskSize,
    font: Option<&SystemFont>,
) -> DecodedImage {
    let out_w = out_w.clamp(1, 4096);
    let out_h = out_h.clamp(1, 4096);
    match src {
        MaskSource::Svg(text) => {
            // Parse the SVG markup into a DOM (the same parser inline <svg> uses) and rasterize its
            // <svg> subtree at the box size. The rasterizer's viewBox fit is xMidYMid-meet (contain),
            // matching the common `mask: ... / contain`. (`stretch`/`cover` differences in aspect are
            // simplified to this contain fit — documented.)
            let doc = html::parse(text);
            let svg_id = (0..doc.len()).map(dom::NodeId).find(|&id| {
                matches!(&doc.get(id).data,
                    dom::NodeData::Element(e) if e.tag.eq_ignore_ascii_case("svg"))
            });
            match svg_id {
                Some(id) => svg::rasterize_svg(&doc, id, out_w, out_h, font),
                None => DecodedImage {
                    rgba: vec![0; (out_w * out_h * 4) as usize],
                    w: out_w,
                    h: out_h,
                },
            }
        }
        MaskSource::Raster(img) => {
            // Fit the decoded raster into the box and sample its alpha into the coverage buffer.
            let mut rgba = vec![0u8; (out_w * out_h * 4) as usize];
            if img.w == 0 || img.h == 0 {
                return DecodedImage {
                    rgba,
                    w: out_w,
                    h: out_h,
                };
            }
            // Destination sub-rect (device px) the mask occupies, per the size keyword.
            let (dx, dy, dw, dh) = match size {
                style::MaskSize::Stretch => (0.0, 0.0, out_w as f32, out_h as f32),
                style::MaskSize::Contain | style::MaskSize::Cover => {
                    let sx = out_w as f32 / img.w as f32;
                    let sy = out_h as f32 / img.h as f32;
                    let s = if matches!(size, style::MaskSize::Cover) {
                        sx.max(sy)
                    } else {
                        sx.min(sy)
                    };
                    let dw = img.w as f32 * s;
                    let dh = img.h as f32 * s;
                    ((out_w as f32 - dw) / 2.0, (out_h as f32 - dh) / 2.0, dw, dh)
                }
            };
            for y in 0..out_h {
                for x in 0..out_w {
                    let fx = x as f32 - dx;
                    let fy = y as f32 - dy;
                    if fx < 0.0 || fy < 0.0 || fx >= dw || fy >= dh {
                        continue; // outside the fitted mask → coverage 0 (already zeroed)
                    }
                    let sx = ((fx / dw) * img.w as f32) as u32;
                    let sy = ((fy / dh) * img.h as f32) as u32;
                    let sx = sx.min(img.w - 1);
                    let sy = sy.min(img.h - 1);
                    let si = ((sy * img.w + sx) * 4) as usize;
                    let a = img.rgba.get(si + 3).copied().unwrap_or(0);
                    let di = ((y * out_w + x) * 4) as usize;
                    rgba[di + 3] = a;
                }
            }
            DecodedImage {
                rgba,
                w: out_w,
                h: out_h,
            }
        }
    }
}

/// Paint a solid color `c` into the box's border box, modulated by a mask coverage bitmap `cov`
/// (whose ALPHA channel is the coverage). Each destination pixel's alpha = `c.a * cov_alpha`, so
/// the result is the background color in the shape of the mask. Axis-aligned only (the common case);
/// under a non-axis-aligned transform we fall back to the device bounding box (an approximation).
fn paint_masked_bg(
    fb: &mut Framebuffer,
    xf: &Affine,
    border: layout::Rect,
    cov: &DecodedImage,
    c: Color,
    axis: bool,
) {
    if cov.w == 0 || cov.h == 0 {
        return;
    }
    // Device-space destination rect of the border box.
    let dst = if axis {
        xf_rect(xf, border.x, border.y, border.width, border.height)
    } else {
        // Bounding box of the four mapped corners.
        let p = [
            xf.apply(border.x, border.y),
            xf.apply(border.x + border.width, border.y),
            xf.apply(border.x, border.y + border.height),
            xf.apply(border.x + border.width, border.y + border.height),
        ];
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (px, py) in p {
            x0 = x0.min(px);
            y0 = y0.min(py);
            x1 = x1.max(px);
            y1 = y1.max(py);
        }
        Rect {
            x: x0.round() as i32,
            y: y0.round() as i32,
            w: (x1 - x0).round() as i32,
            h: (y1 - y0).round() as i32,
        }
    };
    if dst.w <= 0 || dst.h <= 0 {
        return;
    }
    // Sample the coverage bitmap (nearest-neighbour) across the destination rect and blend the
    // color with the modulated alpha at each pixel.
    for oy in 0..dst.h {
        let sy = ((oy as i64 * cov.h as i64) / dst.h as i64).clamp(0, cov.h as i64 - 1) as u32;
        for ox in 0..dst.w {
            let sx = ((ox as i64 * cov.w as i64) / dst.w as i64).clamp(0, cov.w as i64 - 1) as u32;
            let si = ((sy * cov.w + sx) * 4) as usize;
            let cova = cov.rgba.get(si + 3).copied().unwrap_or(0);
            if cova == 0 {
                continue;
            }
            fb.blend_coverage(dst.x + ox, dst.y + oy, cova, c);
        }
    }
}

/// Like [`collect_node_rects`] but records each replaced-image box's **content** rect (device px) —
/// used to rasterize each inline `<svg>` at its exact on-screen size for crisp output.
fn collect_content_rects(b: &layout::LayoutBox, out: &mut HashMap<usize, layout::Rect>) {
    if let (Some(node), layout::BoxContent::Image(_)) = (b.node, &b.content) {
        out.entry(node.0).or_insert(b.dimensions.content);
    }
    for c in &b.children {
        collect_content_rects(c, out);
    }
}

/// Seed `out` with the intrinsic size of every `<canvas>` element: its `width`/`height` attributes,
/// or the spec default 300×150 when absent. Layout treats `<canvas>` as a replaced element and uses
/// this (the same way an `<img>`'s decoded size is used) for aspect-ratio-preserving sizing.
fn collect_canvas_intrinsics(doc: &dom::Document, out: &mut HashMap<dom::NodeId, (f32, f32)>) {
    for i in 0..doc.len() {
        let id = dom::NodeId(i);
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag.eq_ignore_ascii_case("canvas") {
                let w = e
                    .attrs
                    .get("width")
                    .and_then(|v| v.trim().parse::<f32>().ok())
                    .unwrap_or(300.0);
                let h = e
                    .attrs
                    .get("height")
                    .and_then(|v| v.trim().parse::<f32>().ok())
                    .unwrap_or(150.0);
                out.insert(id, (w.max(1.0), h.max(1.0)));
            }
        }
    }
}

/// Seed `out` with the intrinsic size of every inline `<svg>` element: its `width`/`height` attrs,
/// else its `viewBox` width/height, else the spec default 300×150. Layout treats `<svg>` as a
/// replaced element and uses this for sizing (the same way an `<img>`'s decoded size is used).
fn collect_svg_intrinsics(doc: &dom::Document, out: &mut HashMap<dom::NodeId, (f32, f32)>) {
    for i in 0..doc.len() {
        let id = dom::NodeId(i);
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag.eq_ignore_ascii_case("svg") {
                out.insert(id, svg::intrinsic_size(e));
            }
        }
    }
}

/// Format an f32 for embedding in JSON, finite-guarded (NaN/Inf → 0).
fn fnum(v: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

/// True if `id` is an editable text field: a text-like `<input>` (type text/search/email/url/tel/
/// password/number/none) or a `<textarea>`, and not `disabled`/`readonly`. These are the controls
/// that accept typed character input.
fn is_editable_text_field(doc: &dom::Document, id: dom::NodeId) -> bool {
    let el = match &doc.get(id).data {
        dom::NodeData::Element(e) => e,
        _ => return false,
    };
    if el.attrs.contains_key("disabled") || el.attrs.contains_key("readonly") {
        return false;
    }
    if el.tag.eq_ignore_ascii_case("textarea") {
        return true;
    }
    if !el.tag.eq_ignore_ascii_case("input") {
        return false;
    }
    let ty = el
        .attrs
        .get("type")
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ty.as_str(),
        "" | "text" | "search" | "email" | "url" | "tel" | "password" | "number"
    )
}

/// Walk from `node` up the ancestor chain, returning the first node (including `node` itself) that
/// is an editable text field (see [`is_editable_text_field`]), or `None` if none is found.
fn editable_text_ancestor(doc: &dom::Document, node: dom::NodeId) -> Option<dom::NodeId> {
    let mut cur = Some(node);
    while let Some(id) = cur {
        if id.0 >= doc.len() {
            break;
        }
        if is_editable_text_field(doc, id) {
            return Some(id);
        }
        cur = doc.get(id).parent;
    }
    None
}

/// The `value` attribute of an element node, if it is an element (used to detect `change`).
fn node_value(doc: &dom::Document, id: dom::NodeId) -> Option<String> {
    if id.0 >= doc.len() {
        return None;
    }
    match &doc.get(id).data {
        dom::NodeData::Element(e) => Some(e.attrs.get("value").cloned().unwrap_or_default()),
        _ => None,
    }
}

/// Resolve the checkable `<input type=checkbox|radio>` that a click on `node` should toggle, if
/// any: the nearest ancestor-or-self checkable input, OR — when `node` is (inside) a `<label for>`
/// — the input that label points at. Returns `None` for disabled controls or when none is found.
fn checkable_target(doc: &dom::Document, node: dom::NodeId) -> Option<dom::NodeId> {
    fn is_checkable(doc: &dom::Document, id: dom::NodeId) -> bool {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag.eq_ignore_ascii_case("input") && !e.attrs.contains_key("disabled") {
                let ty = e
                    .attrs
                    .get("type")
                    .map(|s| s.trim().to_ascii_lowercase())
                    .unwrap_or_default();
                return ty == "checkbox" || ty == "radio";
            }
        }
        false
    }
    // Ancestor-or-self walk for a checkable input, or a <label for>.
    let mut cur = Some(node);
    while let Some(id) = cur {
        if id.0 >= doc.len() {
            break;
        }
        if is_checkable(doc, id) {
            return Some(id);
        }
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag.eq_ignore_ascii_case("label") {
                if let Some(for_id) = e.attrs.get("for") {
                    if let Some(target) = find_by_attr_id(doc, doc.root(), for_id) {
                        if is_checkable(doc, target) {
                            return Some(target);
                        }
                    }
                }
            }
        }
        cur = doc.get(id).parent;
    }
    None
}

/// If the click landed on (or inside) a `<summary>`, return its nearest ancestor `<details>` so it
/// can be toggled open/closed. `None` otherwise.
fn details_toggle_target(doc: &dom::Document, node: dom::NodeId) -> Option<dom::NodeId> {
    let mut cur = Some(node);
    while let Some(id) = cur {
        if id.0 >= doc.len() {
            break;
        }
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag.eq_ignore_ascii_case("summary") {
                let mut p = doc.get(id).parent;
                while let Some(pid) = p {
                    if pid.0 < doc.len() {
                        if let dom::NodeData::Element(pe) = &doc.get(pid).data {
                            if pe.tag.eq_ignore_ascii_case("details") {
                                return Some(pid);
                            }
                        }
                    }
                    p = doc.get(pid).parent;
                }
                return None;
            }
        }
        cur = doc.get(id).parent;
    }
    None
}

/// Depth-first search for the first element whose `id` attribute equals `id`.
fn find_by_attr_id(doc: &dom::Document, root: dom::NodeId, id: &str) -> Option<dom::NodeId> {
    if root.0 >= doc.len() {
        return None;
    }
    if let dom::NodeData::Element(e) = &doc.get(root).data {
        if e.attrs.get("id").map(String::as_str) == Some(id) {
            return Some(root);
        }
    }
    for &c in &doc.get(root).children {
        if let Some(f) = find_by_attr_id(doc, c, id) {
            return Some(f);
        }
    }
    None
}

/// Test-only: depth-first search for the first element with the given lowercase tag name.
#[cfg(test)]
fn find_tag(doc: &dom::Document, root: dom::NodeId, tag: &str) -> Option<dom::NodeId> {
    if root.0 >= doc.len() {
        return None;
    }
    if let dom::NodeData::Element(e) = &doc.get(root).data {
        if e.tag.eq_ignore_ascii_case(tag) {
            return Some(root);
        }
    }
    for &c in &doc.get(root).children {
        if let Some(f) = find_tag(doc, c, tag) {
            return Some(f);
        }
    }
    None
}

/// True if `id` is a single-line `<input>` (not a `<textarea>`).
fn is_single_line_input(doc: &dom::Document, id: dom::NodeId) -> bool {
    matches!(&doc.get(id).data, dom::NodeData::Element(e) if e.tag.eq_ignore_ascii_case("input"))
}

/// Walk up from `node` to the nearest ancestor `<form>`, if any.
fn ancestor_form(doc: &dom::Document, node: dom::NodeId) -> Option<dom::NodeId> {
    let mut cur = Some(node);
    while let Some(id) = cur {
        if id.0 >= doc.len() {
            break;
        }
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag.eq_ignore_ascii_case("form") {
                return Some(id);
            }
        }
        cur = doc.get(id).parent;
    }
    None
}

/// If the click on `node` lands on (or inside) a submit control — `<input type=submit>`,
/// `<button type=submit>`, or a `<button>` with no/empty `type` — that sits inside a `<form>`,
/// return that nearest ancestor form. Otherwise `None`.
fn submit_target_form(doc: &dom::Document, node: dom::NodeId) -> Option<dom::NodeId> {
    fn is_submit_control(doc: &dom::Document, id: dom::NodeId) -> bool {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.attrs.contains_key("disabled") {
                return false;
            }
            let ty = e
                .attrs
                .get("type")
                .map(|s| s.trim().to_ascii_lowercase())
                .unwrap_or_default();
            if e.tag.eq_ignore_ascii_case("button") {
                // A <button> defaults to type=submit.
                return ty.is_empty() || ty == "submit";
            }
            if e.tag.eq_ignore_ascii_case("input") {
                return ty == "submit";
            }
        }
        false
    }
    // Find the nearest ancestor-or-self submit control.
    let mut cur = Some(node);
    let mut control = None;
    while let Some(id) = cur {
        if id.0 >= doc.len() {
            break;
        }
        if is_submit_control(doc, id) {
            control = Some(id);
            break;
        }
        cur = doc.get(id).parent;
    }
    control.and_then(|c| ancestor_form(doc, c))
}

/// Recursively paint a layout box and its children, translating every box by the fixed
/// pixel offset `(ox, oy)` and vertically clipping to `[clip_top, clip_bottom]`.
///
/// For each box, in order: (a) fill the border box with `background_color` (if any);
/// (b) paint the four border edges; (c) draw text content at the content rect. Then recurse.
#[allow(clippy::too_many_arguments)]
fn paint_box(
    fb: &mut Framebuffer,
    font: &dyn GlyphRasterizer,
    b: &layout::LayoutBox,
    ox: f32,
    oy: f32,
    clip_top: f32,
    clip_bottom: f32,
    images: &HashMap<dom::NodeId, DecodedImage>,
    canvas_bitmaps: &HashMap<dom::NodeId, DecodedImage>,
    svg_bitmaps: &HashMap<dom::NodeId, DecodedImage>,
    mask_bitmaps: &HashMap<dom::NodeId, DecodedImage>,
    sel_ranges: &[Option<(usize, usize)>],
    run_idx: &mut usize,
) {
    // The base device-space transform is a pure translation by the scroll offset. CSS `transform`
    // declarations compose additional affines on top per-box.
    let xf = Affine::translate(ox, oy);
    paint_box_opacity(
        fb,
        font,
        b,
        &xf,
        clip_top,
        clip_bottom,
        images,
        canvas_bitmaps,
        svg_bitmaps,
        mask_bitmaps,
        1.0,
        sel_ranges,
        run_idx,
    );
}

/// A 2D affine mapping a CSS-space point `(x, y)` to a device-space point: `x' = a*x + c*y + e`,
/// `y' = b*x + d*y + f`. Used to remap painted geometry for CSS `transform` (and to carry the
/// scroll translation). Translate + scale stay axis-aligned (painted exactly); rotation/skew make
/// it non-axis-aligned (background/border rects rasterized as transformed quads; text is positioned
/// by the transform but glyphs are not themselves rotated — see [`paint_box_opacity`]).
#[derive(Clone, Copy, Debug)]
struct Affine {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Affine {
    fn translate(tx: f32, ty: f32) -> Affine {
        Affine {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: tx,
            f: ty,
        }
    }
    /// Map a CSS point to device space.
    fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }
    /// `self` ∘ `m`: apply `m` first (in CSS space), then `self`.
    fn then(&self, m: &Affine) -> Affine {
        Affine {
            a: self.a * m.a + self.c * m.b,
            b: self.b * m.a + self.d * m.b,
            c: self.a * m.c + self.c * m.d,
            d: self.b * m.c + self.d * m.d,
            e: self.a * m.e + self.c * m.f + self.e,
            f: self.b * m.e + self.d * m.f + self.f,
        }
    }
    /// True if the linear part has no rotation/skew (axis-aligned: b == c == 0), so a rect stays a
    /// rect and can be filled with the fast axis-aligned primitives.
    fn is_axis_aligned(&self) -> bool {
        self.b.abs() < 1e-4 && self.c.abs() < 1e-4
    }
}

/// Map a CSS-space rect `(x, y, w, h)` through an axis-aligned affine into a device `Rect`.
/// Caller must ensure `xf.is_axis_aligned()`.
fn xf_rect(xf: &Affine, x: f32, y: f32, w: f32, h: f32) -> Rect {
    let (x0, y0) = xf.apply(x, y);
    let (x1, y1) = xf.apply(x + w, y + h);
    let (lx, rx) = (x0.min(x1), x0.max(x1));
    let (ty, by) = (y0.min(y1), y0.max(y1));
    Rect {
        x: lx.round() as i32,
        y: ty.round() as i32,
        w: (rx - lx).round() as i32,
        h: (by - ty).round() as i32,
    }
}

/// Scale a u8 alpha by an effective opacity in 0.0..=1.0.
fn scale_alpha(a: u8, opacity: f32) -> u8 {
    ((a as f32) * opacity.clamp(0.0, 1.0))
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Paint a box, multiplying every painted alpha by `effective_opacity` (the product of this
/// box's and all ancestor `opacity` values). This approximates group opacity without an offscreen
/// layer: each fill/blit/glyph is composited at the scaled alpha rather than the whole subtree
/// being flattened first, so overlapping descendants may show seams — acceptable for our purposes.
#[allow(clippy::too_many_arguments)]
fn paint_box_opacity(
    fb: &mut Framebuffer,
    font: &dyn GlyphRasterizer,
    b: &layout::LayoutBox,
    xf: &Affine,
    clip_top: f32,
    clip_bottom: f32,
    images: &HashMap<dom::NodeId, DecodedImage>,
    canvas_bitmaps: &HashMap<dom::NodeId, DecodedImage>,
    svg_bitmaps: &HashMap<dom::NodeId, DecodedImage>,
    mask_bitmaps: &HashMap<dom::NodeId, DecodedImage>,
    parent_opacity: f32,
    sel_ranges: &[Option<(usize, usize)>],
    run_idx: &mut usize,
) {
    // This box's opacity multiplies into the inherited (effective) opacity for itself + subtree.
    let opacity = parent_opacity * b.style.opacity.clamp(0.0, 1.0);

    let border = b.dimensions.border_box();
    let content = b.dimensions.content;
    let radius = b.style.border_radius();
    let extras = b.style.extras.as_deref();

    // Fast-path: the common no-transform box keeps the incoming affine. A CSS `transform` composes
    // an extra affine pivoted at the transform-origin (in this box's border-box space), so the box
    // *and its whole subtree* are remapped by that affine (translate/scale exactly; rotate/skew via
    // transformed quads for fills).
    let local_xf;
    let xf: &Affine = if let Some(m) = extras.and_then(|e| e.transform) {
        let origin = extras.map(|e| e.transform_origin).unwrap_or((0.5, 0.5));
        let ox = border.x + origin.0 * border.width;
        let oy = border.y + origin.1 * border.height;
        // Resolve percentage translates against the box size (parser left them at 0; CSS uses the
        // element's own size). We re-resolve here by scaling the affine's translate columns — but
        // since the parser already produced absolute px for px translates, only the matrix's e/f
        // (which were 0 for %), this is a no-op for the common case. Keeping it simple: apply m
        // about (ox, oy): T(ox,oy) * m * T(-ox,-oy).
        let pivot = Affine {
            a: m[0],
            b: m[1],
            c: m[2],
            d: m[3],
            e: m[4],
            f: m[5],
        };
        let to_origin = Affine::translate(ox, oy);
        let from_origin = Affine::translate(-ox, -oy);
        local_xf = xf.then(&to_origin.then(&pivot).then(&from_origin));
        &local_xf
    } else {
        xf
    };

    let axis = xf.is_axis_aligned();
    // Device-space vertical extent of the box (for the visible-band clip). With a non-axis-aligned
    // transform we conservatively take the bounding box of the four mapped corners.
    let (top, bottom) = {
        let y0 = border.y.min(content.y);
        let y1 = (border.y + border.height).max(content.y + content.height);
        let x0 = border.x.min(content.x);
        let x1 = (border.x + border.width).max(content.x + content.width);
        let corners = [
            xf.apply(x0, y0),
            xf.apply(x1, y0),
            xf.apply(x0, y1),
            xf.apply(x1, y1),
        ];
        let mut t = f32::MAX;
        let mut bt = f32::MIN;
        for (_, cy) in corners {
            t = t.min(cy);
            bt = bt.max(cy);
        }
        (t, bt)
    };
    let offscreen = bottom < clip_top || top >= clip_bottom;

    // Cull the whole offscreen subtree — don't even recurse — so a long page (e.g. browserscore's
    // 14M-px DOM with every <details> open) doesn't walk thousands of off-viewport boxes per frame.
    // In-flow children are contained in their parent's (affine-mapped) vertical extent, so an
    // offscreen box's descendants are offscreen too. Skipped only when there's no active text
    // selection: the selection highlight indexes text runs by global DFS position, and skipping a
    // subtree would desync that counter (when nothing is selected, `run_idx` is unused).
    if offscreen && sel_ranges.is_empty() {
        return;
    }

    if !offscreen && opacity > 0.0 {
        // (0) OUTER box-shadows: painted BEFORE the background so the box sits on top.
        if let Some(ex) = extras {
            for sh in &ex.box_shadows {
                if !sh.inset {
                    paint_box_shadow(fb, xf, border, radius, sh, opacity, false);
                }
            }
        }

        // (a) Background fills the border box: a gradient if set, else the solid color. When the box
        // carries a `mask-image`, the background is composited only through the mask's opaque pixels
        // (the icon technique: `background: currentColor; mask: url(icon.svg)`) — see `paint_masked_bg`.
        let mask_cov = b.node.and_then(|n| mask_bitmaps.get(&n));
        if let Some(cov) = mask_cov {
            if let Some((r, g, bl)) = b.style.background_color {
                // Masked solid background: stamp the color through the coverage alpha.
                let c = Color {
                    r,
                    g,
                    b: bl,
                    a: scale_alpha(255, opacity),
                };
                paint_masked_bg(fb, xf, border, cov, c, axis);
            } else if let Some(grad) = extras.and_then(|e| e.background_gradient.as_ref()) {
                // Gradient-as-background under a mask is out of scope (the icon technique uses a solid
                // color). Paint the gradient unmasked so something shows. Rare.
                paint_gradient_fill(fb, xf, border, radius, grad, opacity, axis);
            }
        } else if let Some(grad) = extras.and_then(|e| e.background_gradient.as_ref()) {
            paint_gradient_fill(fb, xf, border, radius, grad, opacity, axis);
        } else if let Some((r, g, bl)) = b.style.background_color {
            let c = Color {
                r,
                g,
                b: bl,
                a: scale_alpha(255, opacity),
            };
            fill_box(
                fb,
                xf,
                border.x,
                border.y,
                border.width,
                border.height,
                radius,
                c,
                axis,
            );
        }

        // (b) Borders. For a collapsed table CELL, draw single shared 1px lines: left/top at the
        // border-box edges and right/bottom at the OUTER edge coordinate, so a cell's right line and
        // its flush neighbour's left line land on the same device pixel (a clean single-line grid,
        // not a doubled/gapped pair). Otherwise: the normal four filled edge rects.
        let e = b.dimensions.border;
        let ba = scale_alpha(255, opacity);
        let bc = Color {
            r: b.style.border_color.0,
            g: b.style.border_color.1,
            b: b.style.border_color.2,
            a: ba,
        };
        let collapsed_cell = b.style.is_table_cell
            && b.style.border_collapse == style::BorderCollapse::Collapse
            && (e.top > 0.0 || e.right > 0.0 || e.bottom > 0.0 || e.left > 0.0);
        if collapsed_cell {
            let lw = 1.0f32; // single hairline regardless of declared width (simplified resolution)
                             // left + top at the border-box edges
            fill_box(
                fb,
                xf,
                border.x,
                border.y,
                lw,
                border.height + lw,
                0.0,
                bc,
                axis,
            );
            fill_box(
                fb,
                xf,
                border.x,
                border.y,
                border.width + lw,
                lw,
                0.0,
                bc,
                axis,
            );
            // right + bottom at the OUTER edge coordinate (coincides with the neighbour's left/top)
            fill_box(
                fb,
                xf,
                border.x + border.width,
                border.y,
                lw,
                border.height + lw,
                0.0,
                bc,
                axis,
            );
            fill_box(
                fb,
                xf,
                border.x,
                border.y + border.height,
                border.width + lw,
                lw,
                0.0,
                bc,
                axis,
            );
        } else {
            if e.top > 0.0 {
                fill_box(
                    fb,
                    xf,
                    border.x,
                    border.y,
                    border.width,
                    e.top,
                    radius.min(e.top.max(1.0)),
                    bc,
                    axis,
                );
            }
            if e.bottom > 0.0 {
                fill_box(
                    fb,
                    xf,
                    border.x,
                    border.y + border.height - e.bottom,
                    border.width,
                    e.bottom,
                    radius.min(e.bottom.max(1.0)),
                    bc,
                    axis,
                );
            }
            if e.left > 0.0 {
                fill_box(
                    fb,
                    xf,
                    border.x,
                    border.y,
                    e.left,
                    border.height,
                    0.0,
                    bc,
                    axis,
                );
            }
            if e.right > 0.0 {
                fill_box(
                    fb,
                    xf,
                    border.x + border.width - e.right,
                    border.y,
                    e.right,
                    border.height,
                    0.0,
                    bc,
                    axis,
                );
            }
        }

        // (a2) INSET box-shadows: painted inside the box AFTER the background (best-effort: a
        // feathered inner band, no rounded clipping).
        if let Some(ex) = extras {
            for sh in &ex.box_shadows {
                if sh.inset {
                    paint_box_shadow(fb, xf, border, radius, sh, opacity, true);
                }
            }
        }

        // (c) Text content, at the content rect's baseline. Don't paint into the console area.
        // Text is positioned through the affine's mapped origin; glyphs are not rotated (NOTE:
        // rotated/skewed text is positioned but rendered upright — an approximation).
        if let layout::BoxContent::Text(s) = &b.content {
            let (dx, dy) = xf.apply(content.x, content.y);
            if dy < clip_bottom {
                // Scale font size by the affine's average linear magnitude so scale() enlarges text.
                let sx = (xf.a * xf.a + xf.b * xf.b).sqrt();
                let sy = (xf.c * xf.c + xf.d * xf.d).sqrt();
                let scale = ((sx + sy) * 0.5).max(0.01);
                let fs = b.style.font_size * scale;
                let ta = scale_alpha(255, opacity);
                let color = Color {
                    r: b.style.color.0,
                    g: b.style.color.1,
                    b: b.style.color.2,
                    a: ta,
                };
                let x = dx;
                let baseline = dy + fs * 0.8;
                // Selection highlight: if this run (identified by its DFS index) has a selected
                // character sub-range, fill a translucent rect behind those glyphs BEFORE drawing
                // the text so the glyphs stay legible on top. Advance widths use the SAME scaled
                // font size + letter-spacing the glyph painter uses, so the band lines up exactly.
                if !s.is_empty() {
                    if let Some(Some((cs, ce))) = sel_ranges.get(*run_idx) {
                        let ls = b.style.letter_spacing * scale;
                        let mut hx0 = x;
                        let mut pen = x;
                        for (i, ch) in s.chars().enumerate() {
                            if i == *cs {
                                hx0 = pen;
                            }
                            pen += font.advance(ch, fs) + ls;
                            if i + 1 == *ce {
                                break;
                            }
                        }
                        // If the range starts at 0, hx0 stays at the run's left edge.
                        let hx1 = pen;
                        let top = dy.round() as i32;
                        let h = (fs * 1.25).round().max(1.0) as i32;
                        let w = (hx1 - hx0).round() as i32;
                        if w > 0 {
                            // A translucent macOS-ish selection blue, composited over the text bg.
                            let hl = Color {
                                r: 74,
                                g: 144,
                                b: 255,
                                a: scale_alpha(102, opacity),
                            };
                            fb.fill_rect(
                                Rect {
                                    x: hx0.round() as i32,
                                    y: top,
                                    w,
                                    h,
                                },
                                hl,
                            );
                        }
                    }
                }
                let end_x = draw_run(
                    fb,
                    font,
                    s,
                    x,
                    baseline,
                    fs,
                    color,
                    b.style.bold,
                    b.style.letter_spacing * scale,
                );
                let run_w = (end_x - x).max(0.0);
                if run_w > 0.0 {
                    let thickness = (fs / 14.0).clamp(1.0, 2.0).round().max(1.0) as i32;
                    if b.style.underline {
                        let uy = (baseline + 1.0).round() as i32;
                        fb.fill_rect(
                            Rect {
                                x: x.round() as i32,
                                y: uy,
                                w: run_w.round() as i32,
                                h: thickness,
                            },
                            color,
                        );
                    }
                    if b.style.line_through {
                        let my = (baseline - fs * 0.3).round() as i32;
                        fb.fill_rect(
                            Rect {
                                x: x.round() as i32,
                                y: my,
                                w: run_w.round() as i32,
                                h: thickness,
                            },
                            color,
                        );
                    }
                    if b.style.overline {
                        // A line above the text, near the top of the em box (~0.8em above baseline).
                        let oy = (baseline - fs * 0.8).round() as i32;
                        fb.fill_rect(
                            Rect {
                                x: x.round() as i32,
                                y: oy,
                                w: run_w.round() as i32,
                                h: thickness,
                            },
                            color,
                        );
                    }
                }
            }
        }

        // (c1b) List-item marker: a bullet/number drawn like a text run at the marker's content
        // origin (positioned by layout in the list's left padding). No selection handling.
        if let layout::BoxContent::Marker(s) = &b.content {
            let (dx, dy) = xf.apply(content.x, content.y);
            if dy < clip_bottom && !s.is_empty() {
                let sx = (xf.a * xf.a + xf.b * xf.b).sqrt();
                let sy = (xf.c * xf.c + xf.d * xf.d).sqrt();
                let scale = ((sx + sy) * 0.5).max(0.01);
                let fs = b.style.font_size * scale;
                let ta = scale_alpha(255, opacity);
                let color = Color {
                    r: b.style.color.0,
                    g: b.style.color.1,
                    b: b.style.color.2,
                    a: ta,
                };
                let baseline = dy + fs * 0.8;
                draw_run(
                    fb,
                    font,
                    s,
                    dx,
                    baseline,
                    fs,
                    color,
                    b.style.bold,
                    b.style.letter_spacing * scale,
                );
            }
        }

        // (c2) Caret: the focused-field text cursor. A solid thin bar filling the content rect in
        // the foreground color (mapped through the affine like any other box).
        if matches!(b.content, layout::BoxContent::Caret) {
            let ca = scale_alpha(255, opacity);
            let cc = Color {
                r: b.style.color.0,
                g: b.style.color.1,
                b: b.style.color.2,
                a: ca,
            };
            fill_box(
                fb,
                xf,
                content.x,
                content.y,
                content.width,
                content.height,
                0.0,
                cc,
                axis,
            );
        }

        // (c3) Form widget: a checkbox/radio, range slider, color swatch, or progress/meter bar,
        // drawn as primitive shapes (no glyphs) in the content rect.
        if let layout::BoxContent::Widget(kind) = &b.content {
            paint_widget(fb, xf, content, *kind, opacity);
        }

        // (d) Replaced image content: blit the decoded pixels into the content rect, scaled.
        // (Axis-aligned transforms map the destination rect exactly; rotation is approximated by
        // the bounding box.)
        if let layout::BoxContent::Image(node) = &b.content {
            let dst = xf_rect(xf, content.x, content.y, content.width, content.height);
            if dst.y < clip_bottom as i32 {
                // A <canvas> resolves to its rasterized display-list bitmap, an inline <svg> to its
                // rasterized subtree bitmap, everything else to a decoded <img>. All composite
                // identically.
                match canvas_bitmaps
                    .get(node)
                    .or_else(|| svg_bitmaps.get(node))
                    .or_else(|| images.get(node))
                {
                    Some(img) if opacity >= 0.999 => fb.blit_rgba(dst, &img.rgba, img.w, img.h),
                    Some(img) => {
                        let mut scaled = img.rgba.clone();
                        for px in scaled.chunks_exact_mut(4) {
                            px[3] = scale_alpha(px[3], opacity);
                        }
                        fb.blit_rgba(dst, &scaled, img.w, img.h);
                    }
                    None => {
                        let ph = Color {
                            r: 140,
                            g: 140,
                            b: 150,
                            a: scale_alpha(120, opacity),
                        };
                        if dst.w > 0 && dst.h > 0 {
                            fb.fill_rect(
                                Rect {
                                    x: dst.x,
                                    y: dst.y,
                                    w: dst.w,
                                    h: 1,
                                },
                                ph,
                            );
                            fb.fill_rect(
                                Rect {
                                    x: dst.x,
                                    y: dst.y + dst.h - 1,
                                    w: dst.w,
                                    h: 1,
                                },
                                ph,
                            );
                            fb.fill_rect(
                                Rect {
                                    x: dst.x,
                                    y: dst.y,
                                    w: 1,
                                    h: dst.h,
                                },
                                ph,
                            );
                            fb.fill_rect(
                                Rect {
                                    x: dst.x + dst.w - 1,
                                    y: dst.y,
                                    w: 1,
                                    h: dst.h,
                                },
                                ph,
                            );
                        }
                    }
                }
            }
        }
    }

    // Advance the text-run counter for every non-empty Text run, INDEPENDENT of offscreen culling /
    // opacity, so it stays in lockstep with `collect_text_runs`' DFS order (which the selection
    // ranges were built against).
    if let layout::BoxContent::Text(s) = &b.content {
        if !s.is_empty() {
            *run_idx += 1;
        }
    }

    for child in &b.children {
        paint_box_opacity(
            fb,
            font,
            child,
            xf,
            clip_top,
            clip_bottom,
            images,
            canvas_bitmaps,
            svg_bitmaps,
            mask_bitmaps,
            opacity,
            sel_ranges,
            run_idx,
        );
    }
}

/// Fill a CSS-space rect through an affine: axis-aligned → a (rounded) device rect; otherwise a
/// transformed quad (rounding ignored). `radius` only applies in the axis-aligned case.
fn fill_box(
    fb: &mut Framebuffer,
    xf: &Affine,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    c: Color,
    axis: bool,
) {
    if axis {
        fb.fill_round_rect(xf_rect(xf, x, y, w, h), radius, c);
    } else {
        let p0 = xf.apply(x, y);
        let p1 = xf.apply(x + w, y);
        let p2 = xf.apply(x + w, y + h);
        let p3 = xf.apply(x, y + h);
        fill_quad(fb, [p0, p1, p2, p3], c);
    }
}

/// Stroke a rectangle outline `t` device-px thick (inside the rect's edges), source-over at `c`.
/// Used for widget borders (checkbox/swatch/bar tracks). Coordinates are device px.
fn stroke_rect(fb: &mut Framebuffer, r: Rect, t: i32, c: Color) {
    if r.w <= 0 || r.h <= 0 || t <= 0 {
        return;
    }
    let t = t.min(r.w).min(r.h);
    fb.fill_rect(
        Rect {
            x: r.x,
            y: r.y,
            w: r.w,
            h: t,
        },
        c,
    ); // top
    fb.fill_rect(
        Rect {
            x: r.x,
            y: r.y + r.h - t,
            w: r.w,
            h: t,
        },
        c,
    ); // bottom
    fb.fill_rect(
        Rect {
            x: r.x,
            y: r.y,
            w: t,
            h: r.h,
        },
        c,
    ); // left
    fb.fill_rect(
        Rect {
            x: r.x + r.w - t,
            y: r.y,
            w: t,
            h: r.h,
        },
        c,
    ); // right
}

/// Fill a circle (device-space center `cx,cy`, radius `rad`) source-over at `c`, with 1px AA at the
/// rim. Used for radio dials and range thumbs.
fn fill_circle(fb: &mut Framebuffer, cx: f32, cy: f32, rad: f32, c: Color) {
    if rad <= 0.0 {
        return;
    }
    let x0 = (cx - rad).floor() as i32;
    let x1 = (cx + rad).ceil() as i32;
    let y0 = (cy - rad).floor() as i32;
    let y1 = (cy + rad).ceil() as i32;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let d = (dx * dx + dy * dy).sqrt();
            // Full coverage inside (rad-1), linear AA over the outer 1px band.
            let cov = (rad - d + 0.5).clamp(0.0, 1.0);
            if cov > 0.0 {
                // blend_coverage folds c.a in itself; pass coverage as 0..=255.
                fb.blend_coverage(x, y, (cov * 255.0).round() as u8, c);
            }
        }
    }
}

/// Draw a 1px-ish line between two device-space points (Bresenham, `t`-px square brush) at `c`.
/// Used for the checkbox check mark's two strokes.
fn draw_line(fb: &mut Framebuffer, x0: f32, y0: f32, x1: f32, y1: f32, t: i32, c: Color) {
    let t = t.max(1);
    let (mut x0, mut y0) = (x0.round() as i32, y0.round() as i32);
    let (x1, y1) = (x1.round() as i32, y1.round() as i32);
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        fb.fill_rect(
            Rect {
                x: x0 - t / 2,
                y: y0 - t / 2,
                w: t,
                h: t,
            },
            c,
        );
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

/// Paint a drawn form widget into its (CSS-space) content rect, mapped through the affine. Widgets
/// are primitive shapes only (no glyphs): a checkbox/radio, a range slider, a color swatch, or a
/// progress/meter bar. Sized at layout-build time; here we just rasterize.
fn paint_widget(
    fb: &mut Framebuffer,
    xf: &Affine,
    content: layout::Rect,
    kind: layout::WidgetKind,
    opacity: f32,
) {
    let a = scale_alpha(255, opacity);
    // Device-space content rect.
    let r = xf_rect(xf, content.x, content.y, content.width, content.height);
    if r.w <= 0 || r.h <= 0 {
        return;
    }
    let border = Color {
        r: 118,
        g: 118,
        b: 118,
        a,
    }; // #767676, the UA control border
    match kind {
        layout::WidgetKind::Checkbox { checked } => {
            // A square box (centered in the content rect), white fill, gray border; a check mark
            // (two strokes) when checked. The font lacks ☑/☐ so we draw it.
            let s = r.w.min(r.h);
            let bx = r.x + (r.w - s) / 2;
            let by = r.y + (r.h - s) / 2;
            let sq = Rect {
                x: bx,
                y: by,
                w: s,
                h: s,
            };
            fb.fill_round_rect(
                sq,
                2.0,
                Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a,
                },
            );
            stroke_rect(fb, sq, 1, border);
            if checked {
                // A blue-ish fill behind a white check, or just a dark check on white. We draw a
                // filled accent box + white check for a clear "on" state.
                let inset = (s as f32 * 0.0).max(0.0) as i32;
                let acc = Rect {
                    x: bx + inset,
                    y: by + inset,
                    w: s - 2 * inset,
                    h: s - 2 * inset,
                };
                fb.fill_round_rect(
                    acc,
                    2.0,
                    Color {
                        r: 36,
                        g: 110,
                        b: 230,
                        a,
                    },
                ); // accent blue
                let fx = bx as f32;
                let fy = by as f32;
                let fs = s as f32;
                let check = Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a,
                };
                let t = (fs / 7.0).round().clamp(1.0, 3.0) as i32;
                // ✓ : down-stroke from (0.22,0.52) to (0.42,0.74), up-stroke to (0.78,0.28).
                draw_line(
                    fb,
                    fx + fs * 0.24,
                    fy + fs * 0.52,
                    fx + fs * 0.43,
                    fy + fs * 0.72,
                    t,
                    check,
                );
                draw_line(
                    fb,
                    fx + fs * 0.43,
                    fy + fs * 0.72,
                    fx + fs * 0.76,
                    fy + fs * 0.30,
                    t,
                    check,
                );
            }
        }
        layout::WidgetKind::Radio { checked } => {
            // A circle outline; a filled inner dot when checked.
            let s = r.w.min(r.h);
            let cx = r.x as f32 + r.w as f32 / 2.0;
            let cy = r.y as f32 + r.h as f32 / 2.0;
            let rad = s as f32 / 2.0;
            fill_circle(
                fb,
                cx,
                cy,
                rad,
                Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a,
                },
            );
            // Border ring: a slightly larger circle in border color, then the white re-fill.
            fill_circle(fb, cx, cy, rad, border);
            fill_circle(
                fb,
                cx,
                cy,
                (rad - 1.0).max(0.5),
                Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a,
                },
            );
            if checked {
                fill_circle(
                    fb,
                    cx,
                    cy,
                    (rad * 0.5).max(1.0),
                    Color {
                        r: 36,
                        g: 110,
                        b: 230,
                        a,
                    },
                );
            }
        }
        layout::WidgetKind::Range { fraction } => {
            // A thin rounded track centered vertically, with a circular thumb at `fraction`.
            let track_h = (r.h / 4).max(3);
            let ty = r.y + (r.h - track_h) / 2;
            let pad = r.h / 2; // keep the thumb inside the box at the extremes
            let track = Rect {
                x: r.x + pad,
                y: ty,
                w: (r.w - 2 * pad).max(1),
                h: track_h,
            };
            fb.fill_round_rect(
                track,
                track_h as f32 / 2.0,
                Color {
                    r: 180,
                    g: 180,
                    b: 180,
                    a,
                },
            );
            // Filled (left) portion in the accent color.
            let filled_w = (track.w as f32 * fraction.clamp(0.0, 1.0)).round() as i32;
            if filled_w > 0 {
                fb.fill_round_rect(
                    Rect {
                        x: track.x,
                        y: ty,
                        w: filled_w,
                        h: track_h,
                    },
                    track_h as f32 / 2.0,
                    Color {
                        r: 36,
                        g: 110,
                        b: 230,
                        a,
                    },
                );
            }
            let thumb_cx = track.x as f32 + track.w as f32 * fraction.clamp(0.0, 1.0);
            let thumb_cy = r.y as f32 + r.h as f32 / 2.0;
            let thumb_r = (r.h as f32 / 2.0 - 1.0).max(3.0);
            fill_circle(
                fb,
                thumb_cx,
                thumb_cy,
                thumb_r,
                Color {
                    r: 240,
                    g: 240,
                    b: 240,
                    a,
                },
            );
            fill_circle(fb, thumb_cx, thumb_cy, thumb_r, border);
            fill_circle(
                fb,
                thumb_cx,
                thumb_cy,
                (thumb_r - 1.0).max(1.0),
                Color {
                    r: 245,
                    g: 245,
                    b: 245,
                    a,
                },
            );
        }
        layout::WidgetKind::Color { rgb } => {
            // A swatch filled with the chosen color, thin gray border (with a small inner inset so
            // the color reads clearly).
            stroke_rect(fb, r, 1, border);
            let inner = Rect {
                x: r.x + 2,
                y: r.y + 2,
                w: (r.w - 4).max(1),
                h: (r.h - 4).max(1),
            };
            fb.fill_rect(
                inner,
                Color {
                    r: rgb.0,
                    g: rgb.1,
                    b: rgb.2,
                    a,
                },
            );
        }
        layout::WidgetKind::Progress { fraction } => {
            // A rounded track (light bg) with a blue filled portion = fraction. Indeterminate
            // (None) → a fully filled track (a reasonable static stand-in for the animation).
            let rad = (r.h as f32 / 2.0).min(6.0);
            fb.fill_round_rect(
                r,
                rad,
                Color {
                    r: 225,
                    g: 225,
                    b: 225,
                    a,
                },
            );
            stroke_rect(
                fb,
                r,
                1,
                Color {
                    r: 190,
                    g: 190,
                    b: 190,
                    a,
                },
            );
            let frac = fraction.unwrap_or(1.0).clamp(0.0, 1.0);
            let fw = (r.w as f32 * frac).round() as i32;
            if fw > 0 {
                fb.fill_round_rect(
                    Rect {
                        x: r.x,
                        y: r.y,
                        w: fw,
                        h: r.h,
                    },
                    rad,
                    Color {
                        r: 36,
                        g: 110,
                        b: 230,
                        a,
                    },
                );
            }
        }
        layout::WidgetKind::Meter { fraction } => {
            // Like progress but a greenish fill.
            let rad = (r.h as f32 / 2.0).min(6.0);
            fb.fill_round_rect(
                r,
                rad,
                Color {
                    r: 225,
                    g: 225,
                    b: 225,
                    a,
                },
            );
            stroke_rect(
                fb,
                r,
                1,
                Color {
                    r: 190,
                    g: 190,
                    b: 190,
                    a,
                },
            );
            let fw = (r.w as f32 * fraction.clamp(0.0, 1.0)).round() as i32;
            if fw > 0 {
                fb.fill_round_rect(
                    Rect {
                        x: r.x,
                        y: r.y,
                        w: fw,
                        h: r.h,
                    },
                    rad,
                    Color {
                        r: 76,
                        g: 174,
                        b: 80,
                        a,
                    },
                );
            }
        }
    }
}

/// Rasterize a (convex) quadrilateral given its 4 device-space corners (in order), source-over at
/// `c`. Used to paint rotated/skewed backgrounds and borders. Scanline fill over the bounding box,
/// testing each pixel center for inclusion (consistent winding). Used only off the no-transform
/// fast path.
fn fill_quad(fb: &mut Framebuffer, pts: [(f32, f32); 4], c: Color) {
    let xs = [pts[0].0, pts[1].0, pts[2].0, pts[3].0];
    let ys = [pts[0].1, pts[1].1, pts[2].1, pts[3].1];
    let minx = xs.iter().cloned().fold(f32::MAX, f32::min).floor().max(0.0) as i32;
    let maxx = xs
        .iter()
        .cloned()
        .fold(f32::MIN, f32::max)
        .ceil()
        .min(fb.width as f32) as i32;
    let miny = ys.iter().cloned().fold(f32::MAX, f32::min).floor().max(0.0) as i32;
    let maxy = ys
        .iter()
        .cloned()
        .fold(f32::MIN, f32::max)
        .ceil()
        .min(fb.height as f32) as i32;
    if maxx <= minx || maxy <= miny {
        return;
    }
    // Sign of the cross product of each edge with the point; convex quad → all same sign inside.
    let inside = |px: f32, py: f32| -> bool {
        let mut sign = 0.0_f32;
        for i in 0..4 {
            let (ax, ay) = pts[i];
            let (bx, by) = pts[(i + 1) % 4];
            let cross = (bx - ax) * (py - ay) - (by - ay) * (px - ax);
            if cross.abs() > 1e-6 {
                if sign == 0.0 {
                    sign = cross.signum();
                } else if cross.signum() != sign {
                    return false;
                }
            }
        }
        true
    };
    for y in miny..maxy {
        let py = y as f32 + 0.5;
        for x in minx..maxx {
            let px = x as f32 + 0.5;
            if inside(px, py) {
                let i = (y as u32 * fb.stride) as usize + (x as usize) * 4;
                blend_pixel(&mut fb.pixels[i..i + 4], c);
            }
        }
    }
}

/// Source-over one color onto a device pixel (mirrors paint's internal blend, exposed for the
/// engine's quad/gradient/shadow rasterizers).
fn blend_pixel(dst: &mut [u8], src: Color) {
    if src.a == 0 {
        return;
    }
    if src.a == 255 {
        dst[0] = src.r;
        dst[1] = src.g;
        dst[2] = src.b;
        dst[3] = 255;
        return;
    }
    let sa = src.a as u32;
    let ia = 255 - sa;
    dst[0] = ((src.r as u32 * sa + dst[0] as u32 * ia) / 255) as u8;
    dst[1] = ((src.g as u32 * sa + dst[1] as u32 * ia) / 255) as u8;
    dst[2] = ((src.b as u32 * sa + dst[2] as u32 * ia) / 255) as u8;
    dst[3] = (sa + dst[3] as u32 * ia / 255).min(255) as u8;
}

/// Fill a box's border-box with a gradient. Each device pixel inside the (axis-aligned) destination
/// rect is mapped back to the box's local 0..1 space, its gradient parameter `t` computed (linear:
/// projection onto the angle vector; radial: normalized distance from center), and the surrounding
/// color stops lerped. Respects `border_radius` (corner clipping like the solid fill) and `opacity`.
/// Non-axis-aligned transforms fall back to the bounding-box rect (rotation of the gradient itself
/// is approximate).
fn paint_gradient_fill(
    fb: &mut Framebuffer,
    xf: &Affine,
    border: layout::Rect,
    radius: f32,
    grad: &style::Gradient,
    opacity: f32,
    _axis: bool,
) {
    let dst = xf_rect(xf, border.x, border.y, border.width, border.height);
    let x0 = dst.x.max(0);
    let y0 = dst.y.max(0);
    let x1 = (dst.x + dst.w).min(fb.width as i32);
    let y1 = (dst.y + dst.h).min(fb.height as i32);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let dw = (dst.w.max(1)) as f32;
    let dh = (dst.h.max(1)) as f32;
    let r = radius
        .min(dst.w as f32 / 2.0)
        .min(dst.h as f32 / 2.0)
        .max(0.0);
    // Linear gradient axis direction in normalized box space (CSS angle: 0=up, 90=right).
    let (dirx, diry, half_len);
    match grad {
        style::Gradient::Linear { angle_deg, .. } => {
            let a = angle_deg.to_radians();
            // Direction the gradient progresses (toward 100%).
            dirx = a.sin();
            diry = -a.cos();
            // Projection length so the gradient spans corner-to-corner along the axis.
            half_len = (dw * dirx.abs() + dh * diry.abs()) * 0.5;
        }
        style::Gradient::Radial { .. } => {
            dirx = 0.0;
            diry = 0.0;
            half_len = (dw * dw + dh * dh).sqrt() * 0.5;
        }
    }
    let cx = (x0 + x1) as f32 * 0.5;
    let cy = (y0 + y1) as f32 * 0.5;
    let stops = match grad {
        style::Gradient::Linear { stops, .. } => stops,
        style::Gradient::Radial { stops } => stops,
    };
    for y in y0..y1 {
        let py = y as f32 + 0.5;
        let row = (y as u32 * fb.stride) as usize;
        for x in x0..x1 {
            let px = x as f32 + 0.5;
            // Rounded-corner clip (matches fill_round_rect).
            if r > 0.0 && !inside_round_rect(px, py, dst, r) {
                continue;
            }
            let t = match grad {
                style::Gradient::Linear { .. } => {
                    let proj = (px - cx) * dirx + (py - cy) * diry;
                    if half_len > 0.0 {
                        (proj / half_len) * 0.5 + 0.5
                    } else {
                        0.5
                    }
                }
                style::Gradient::Radial { .. } => {
                    let dist = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
                    if half_len > 0.0 {
                        dist / half_len
                    } else {
                        0.0
                    }
                }
            };
            let col = sample_stops(stops, t.clamp(0.0, 1.0));
            let a = scale_alpha(col.a, opacity);
            if a == 0 {
                continue;
            }
            let i = row + (x as usize) * 4;
            blend_pixel(
                &mut fb.pixels[i..i + 4],
                Color {
                    r: col.r,
                    g: col.g,
                    b: col.b,
                    a,
                },
            );
        }
    }
}

/// True if a pixel center lies inside a rounded rect (used to clip the gradient/shadow corners).
fn inside_round_rect(px: f32, py: f32, rect: Rect, r: f32) -> bool {
    let left_cx = rect.x as f32 + r;
    let right_cx = (rect.x + rect.w) as f32 - r;
    let top_cy = rect.y as f32 + r;
    let bottom_cy = (rect.y + rect.h) as f32 - r;
    let cx = if px < left_cx {
        Some(left_cx)
    } else if px > right_cx {
        Some(right_cx)
    } else {
        None
    };
    let cy = if py < top_cy {
        Some(top_cy)
    } else if py > bottom_cy {
        Some(bottom_cy)
    } else {
        None
    };
    match (cx, cy) {
        (Some(cx), Some(cy)) => ((px - cx).powi(2) + (py - cy).powi(2)).sqrt() <= r,
        _ => true,
    }
}

/// Linearly interpolate the gradient stops at parameter `t` in 0..1 (stops sorted by `pos`).
fn sample_stops(stops: &[style::GradientStop], t: f32) -> style::Rgba {
    if stops.is_empty() {
        return style::Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        };
    }
    if t <= stops[0].pos {
        return stops[0].color;
    }
    let last = stops.len() - 1;
    if t >= stops[last].pos {
        return stops[last].color;
    }
    for i in 0..last {
        let a = stops[i];
        let b = stops[i + 1];
        if t >= a.pos && t <= b.pos {
            let span = (b.pos - a.pos).max(1e-6);
            let f = (t - a.pos) / span;
            return style::Rgba {
                r: lerp_u8(a.color.r, b.color.r, f),
                g: lerp_u8(a.color.g, b.color.g, f),
                b: lerp_u8(a.color.b, b.color.b, f),
                a: lerp_u8(a.color.a, b.color.a, f),
            };
        }
    }
    stops[last].color
}

fn lerp_u8(a: u8, b: u8, f: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * f)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Paint one box-shadow layer. OUTER: a rect offset by (dx,dy), inflated by `spread`, with a `blur`
/// px feathered edge (concentric alpha-decreasing bands approximating a Gaussian falloff). INSET:
/// a feathered inner band along the box edges. Border-radius rounding is honored for the solid
/// core (corner clip) but the feather is rectangular. Honors `opacity`.
fn paint_box_shadow(
    fb: &mut Framebuffer,
    xf: &Affine,
    border: layout::Rect,
    radius: f32,
    sh: &style::BoxShadow,
    opacity: f32,
    inset: bool,
) {
    let base_a = scale_alpha(sh.color.a, opacity);
    if base_a == 0 {
        return;
    }
    let col = |a: u8| Color {
        r: sh.color.r,
        g: sh.color.g,
        b: sh.color.b,
        a,
    };
    if !inset {
        // Outer: core rect = border box, offset by (dx,dy), inflated by spread.
        let bx = border.x + sh.dx - sh.spread;
        let by = border.y + sh.dy - sh.spread;
        let bw = border.width + 2.0 * sh.spread;
        let bh = border.height + 2.0 * sh.spread;
        let core = xf_rect(xf, bx, by, bw, bh);
        let blur = sh.blur.max(0.0);
        if blur <= 0.5 {
            // Hard shadow: a single solid (rounded) rect.
            fb.fill_round_rect(core, radius, col(base_a));
            return;
        }
        // Feather: draw the core, then expand outward in 1px bands with linearly falling alpha.
        let bands = blur.ceil() as i32;
        fb.fill_round_rect(core, radius, col(base_a));
        for k in 1..=bands {
            let frac = 1.0 - (k as f32 / (bands as f32 + 1.0));
            let a = (base_a as f32 * frac * 0.6).round() as u8;
            if a == 0 {
                continue;
            }
            let ring = Rect {
                x: core.x - k,
                y: core.y - k,
                w: core.w + 2 * k,
                h: core.h + 2 * k,
            };
            // Draw just the 1px ring (top/bottom/left/right strips) to avoid re-darkening the core.
            fb.fill_rect(
                Rect {
                    x: ring.x,
                    y: ring.y,
                    w: ring.w,
                    h: 1,
                },
                col(a),
            );
            fb.fill_rect(
                Rect {
                    x: ring.x,
                    y: ring.y + ring.h - 1,
                    w: ring.w,
                    h: 1,
                },
                col(a),
            );
            fb.fill_rect(
                Rect {
                    x: ring.x,
                    y: ring.y,
                    w: 1,
                    h: ring.h,
                },
                col(a),
            );
            fb.fill_rect(
                Rect {
                    x: ring.x + ring.w - 1,
                    y: ring.y,
                    w: 1,
                    h: ring.h,
                },
                col(a),
            );
        }
    } else {
        // Inset (best-effort): a feathered band just inside the border box, offset by (dx,dy).
        let inner = xf_rect(xf, border.x, border.y, border.width, border.height);
        let blur = sh.blur.max(1.0);
        let bands = (blur + sh.spread.abs()).ceil().max(1.0) as i32;
        for k in 0..bands {
            let frac = 1.0 - (k as f32 / (bands as f32));
            let a = (base_a as f32 * frac * 0.5).round() as u8;
            if a == 0 {
                continue;
            }
            let dxk = sh.dx.round() as i32;
            let dyk = sh.dy.round() as i32;
            // Top & left bands shift with the offset.
            fb.fill_rect(
                Rect {
                    x: inner.x + dxk,
                    y: inner.y + k + dyk,
                    w: inner.w,
                    h: 1,
                },
                col(a),
            );
            fb.fill_rect(
                Rect {
                    x: inner.x + k + dxk,
                    y: inner.y + dyk,
                    w: 1,
                    h: inner.h,
                },
                col(a),
            );
            fb.fill_rect(
                Rect {
                    x: inner.x + dxk,
                    y: inner.y + inner.h - 1 - k + dyk,
                    w: inner.w,
                    h: 1,
                },
                col(a),
            );
            fb.fill_rect(
                Rect {
                    x: inner.x + inner.w - 1 - k + dxk,
                    y: inner.y + dyk,
                    w: 1,
                    h: inner.h,
                },
                col(a),
            );
        }
    }
}

/// Tags whose subtrees contribute no visible text.
const SKIP_SUBTREE: &[&str] = &["script", "style", "head", "title", "noscript"];

/// Block-ish tags that introduce a line break around their content.
const BLOCK_TAGS: &[&str] = &[
    "p", "div", "h1", "h2", "h3", "h4", "h5", "h6", "li", "br", "section", "article", "header",
    "footer", "ul", "ol", "tr",
];

/// Walk the DOM depth-first and collect visible text, skipping non-rendered subtrees,
/// collapsing ASCII whitespace runs to single spaces, and inserting `\n` around block
/// elements. The result is a reasonable approximation of the page's plain text.
pub fn extract_visible_text(doc: &dom::Document) -> String {
    let mut out = String::new();
    collect_text(doc, doc.root(), &mut out);
    collapse_whitespace(&out)
}

/// Maximum number of external stylesheets fetched per page (including transitively `@import`ed
/// files); the rest are skipped with a note. Sized to accommodate `@import` manifests (a single
/// `<link>` can pull in many component CSS files) while still capping runaway / cyclic imports.
const MAX_EXTERNAL_STYLESHEETS: usize = 100;
/// Maximum number of external scripts fetched per page; the rest are skipped with a note.
const MAX_EXTERNAL_SCRIPTS: usize = 24;
/// Skip fetched script bodies larger than this (mirrors the inline-script cap). Large SPA
/// frameworks ship multi-MB bundles (e.g. youtube's main app bundle is ~10.5 MB), so the cap is
/// generous; V8 parses lazily and the per-run execution budget bounds the time.
const MAX_SCRIPT_BYTES: usize = 32 * 1024 * 1024;

/// One author stylesheet source in document order: either an inline `<style>` body or an
/// external `<link rel=stylesheet href>` whose `href` resolved to an absolute URL.
#[derive(Debug, PartialEq, Eq)]
pub enum StyleSource {
    Inline(String),
    External(String),
}

/// Build the host `request_fetcher` capability passed into the JS runtime: it backs the rewritten
/// JS `fetch()` (arbitrary method + headers + body). Given `(method, resolved_url, body,
/// headers_json)` it parses the headers JSON object, issues the request via [`net::request`], and
/// returns a JSON *envelope* string the JS side parses into a `Response`. Returns `None` on
/// transport error (→ `fetch` rejects with `TypeError`). Runs on the JS worker thread; blocking is
/// fine there.
fn build_request_fetcher(
) -> std::sync::Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync> {
    std::sync::Arc::new(|method: &str, url: &str, body: &str, headers_json: &str| {
        let headers = parse_headers_json(headers_json);
        let body_opt: Option<&[u8]> = if body.is_empty() {
            None
        } else {
            Some(body.as_bytes())
        };
        let resp = net::request(method, url, body_opt, &headers).ok()?;
        let ok = (200..300).contains(&resp.status);
        let status_text = reason_phrase(resp.status);
        let body_str = String::from_utf8_lossy(&resp.body);
        Some(build_response_envelope(
            ok,
            resp.status,
            status_text,
            &resp.final_url,
            &resp.content_type,
            &body_str,
        ))
    })
}

/// Build the host WebSocket *connector* passed into the JS [`js::Session`]: it backs the real
/// `WebSocket` class. Given `(url, id, evt_tx)` it spawns a dedicated thread running [`net::ws_run`]
/// for the lifetime of that socket and returns the `out` sender the JS side uses to send/close.
/// Returns `Err` only if the thread can't be spawned (in which case the JS object fires
/// onerror/onclose synthetically). Crosses the `js` crate boundary with PRIMITIVE tuple channels
/// only (just like `request_fetcher`), so `js` never depends on `net`.
///
/// Tuple protocol (see [`net::ws_run`]): events `(id, kind, payload)` flow over `evt_tx`; outgoing
/// commands `(kind, payload)` flow over the returned sender.
type WsConnector = std::sync::Arc<
    dyn Fn(
            String,
            u64,
            std::sync::mpsc::Sender<(u64, u8, String)>,
        ) -> Result<std::sync::mpsc::Sender<(u8, String)>, String>
        + Send
        + Sync,
>;

fn build_ws_connector() -> WsConnector {
    std::sync::Arc::new(
        |url: String,
         id: u64,
         evt_tx: std::sync::mpsc::Sender<(u64, u8, String)>|
         -> Result<std::sync::mpsc::Sender<(u8, String)>, String> {
            // The JS side sends/closes through `out_tx`; the worker thread owns `out_rx`.
            let (out_tx, out_rx) = std::sync::mpsc::channel::<(u8, String)>();
            std::thread::Builder::new()
                .name("ws".to_string())
                .spawn(move || {
                    net::ws_run(url, id, evt_tx, out_rx);
                })
                .map_err(|e| format!("could not start WebSocket thread: {e}"))?;
            Ok(out_tx)
        },
    )
}

/// Parse a flat JSON object of `name -> string-value` (the headers JSON `fetch` builds with
/// `JSON.stringify`) into a `Vec<(name, value)>`. Tolerant: returns an empty vec on any parse
/// problem (a malformed header map shouldn't abort the request).
fn parse_headers_json(s: &str) -> Vec<(String, String)> {
    let s = s.trim();
    let inner = match s.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
        Some(i) => i.trim(),
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    let bytes = inner.as_bytes();
    let mut i = 0usize;
    // Parse a JSON string starting at `bytes[i] == '"'`, returning (decoded, next_index).
    fn parse_str(bytes: &[u8], mut i: usize) -> Option<(String, usize)> {
        if bytes.get(i) != Some(&b'"') {
            return None;
        }
        i += 1;
        let mut out = String::new();
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'\\' {
                i += 1;
                match bytes.get(i)? {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'u' => {
                        let hex = std::str::from_utf8(bytes.get(i + 1..i + 5)?).ok()?;
                        let cp = u32::from_str_radix(hex, 16).ok()?;
                        out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                        i += 4;
                    }
                    other => out.push(*other as char),
                }
                i += 1;
            } else if c == b'"' {
                return Some((out, i + 1));
            } else {
                // Copy a UTF-8 byte run up to the next escape/quote.
                let start = i;
                while i < bytes.len() && bytes[i] != b'\\' && bytes[i] != b'"' {
                    i += 1;
                }
                out.push_str(std::str::from_utf8(&bytes[start..i]).ok()?);
            }
        }
        None
    }
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let (key, ni) = match parse_str(bytes, i) {
            Some(r) => r,
            None => break,
        };
        i = ni;
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b':') {
            i += 1;
        }
        let (val, ni) = match parse_str(bytes, i) {
            Some(r) => r,
            None => break,
        };
        i = ni;
        out.push((key, val));
        while i < bytes.len() && (bytes[i] == b',' || (bytes[i] as char).is_whitespace()) {
            i += 1;
        }
    }
    out
}

/// JSON-escape a string into `out` (control chars, quotes, backslash). No surrounding quotes.
/// A `"`-quoted, escaped JSON string literal.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    json_escape(s, &mut out);
    out.push('"');
    out
}

fn json_escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

/// Build the JSON response envelope the JS `fetch()` parses into a `Response`.
fn build_response_envelope(
    ok: bool,
    status: u16,
    status_text: &str,
    url: &str,
    content_type: &str,
    body: &str,
) -> String {
    let mut s = String::with_capacity(body.len() + 128);
    s.push_str("{\"ok\":");
    s.push_str(if ok { "true" } else { "false" });
    s.push_str(",\"status\":");
    s.push_str(&status.to_string());
    s.push_str(",\"statusText\":\"");
    json_escape(status_text, &mut s);
    s.push_str("\",\"url\":\"");
    json_escape(url, &mut s);
    s.push_str("\",\"contentType\":\"");
    json_escape(content_type, &mut s);
    s.push_str("\",\"body\":\"");
    json_escape(body, &mut s);
    s.push_str("\"}");
    s
}

/// A minimal reason-phrase for common HTTP status codes (empty when unknown).
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}

/// Resolve `href` against `base` using the `url` crate, returning an absolute
/// `http(s)`/`file` URL. Returns `None` for empty/fragment-only hrefs and for non-fetchable
/// schemes (`data:`, `javascript:`, `mailto:`, …) or anything that fails to parse/join.
pub fn resolve_url(base: &str, href: &str) -> Option<String> {
    let trimmed = href.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    for bad in ["javascript:", "data:", "mailto:", "tel:", "blob:", "about:"] {
        if lower.starts_with(bad) {
            return None;
        }
    }
    let base = url::Url::parse(base).ok()?;
    let joined = base.join(trimmed).ok()?;
    match joined.scheme() {
        "http" | "https" | "file" => Some(joined.into()),
        _ => None,
    }
}

/// Determine the page's base URL: the response's `final_url`, overridden by the `href` of the
/// first `<base href>` element (resolved against `final_url`) if one is present.
pub fn base_url(doc: &dom::Document, final_url: &str) -> String {
    fn find_base(doc: &dom::Document, id: dom::NodeId) -> Option<String> {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag == "base" {
                if let Some(href) = e.attrs.get("href") {
                    return Some(href.clone());
                }
            }
        }
        for &child in &doc.get(id).children {
            if let Some(h) = find_base(doc, child) {
                return Some(h);
            }
        }
        None
    }
    match find_base(doc, doc.root()) {
        Some(href) => resolve_url(final_url, &href).unwrap_or_else(|| final_url.to_string()),
        None => final_url.to_string(),
    }
}

/// Walk the DOM in document order, classifying each author style contribution as an inline
/// `<style>` body or an external `<link rel=stylesheet href>` (resolved against `base`).
/// Pure: no fetching, so the ordering/classification is unit-testable without network.
pub fn collect_style_sources(doc: &dom::Document, base: &str) -> Vec<StyleSource> {
    fn walk(doc: &dom::Document, id: dom::NodeId, base: &str, out: &mut Vec<StyleSource>) {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            match e.tag.as_str() {
                "style" => {
                    let mut src = String::new();
                    for &child in &doc.get(id).children {
                        if let dom::NodeData::Text(t) = &doc.get(child).data {
                            src.push_str(t);
                        }
                    }
                    out.push(StyleSource::Inline(src));
                    return;
                }
                "link" => {
                    let rel = e.attrs.get("rel").map(String::as_str).unwrap_or("");
                    let is_sheet = rel
                        .split_whitespace()
                        .any(|t| t.eq_ignore_ascii_case("stylesheet"));
                    if is_sheet {
                        if let Some(href) = e.attrs.get("href") {
                            if let Some(abs) = resolve_url(base, href) {
                                out.push(StyleSource::External(abs));
                            }
                        }
                    }
                    return;
                }
                _ => {}
            }
        }
        for &child in &doc.get(id).children {
            walk(doc, child, base, out);
        }
    }
    let mut out = Vec::new();
    walk(doc, doc.root(), base, &mut out);
    out
}

/// Collect author stylesheets in document order: inline `<style>` bodies parsed directly, and
/// external `<link rel=stylesheet>` sheets fetched (against `base`) then parsed. Returns the
/// ordered sheets plus any console notes (skipped/failed/over-limit). External fetches are
/// sequential. The cascade order UA < these (DOM order) < inline `style=""` is preserved
/// because this list is interleaved by document position.
pub fn collect_stylesheets(doc: &dom::Document, base: &str) -> (Vec<css::Stylesheet>, Vec<String>) {
    let mut sheets = Vec::new();
    let mut console = Vec::new();
    let mut fetched = 0usize;
    // URLs already fetched (across all sources) so a file imported twice isn't refetched, and
    // import cycles terminate.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for source in collect_style_sources(doc, base) {
        match source {
            StyleSource::Inline(src) => {
                // Inline `<style>` may itself `@import` (rare, but cheap): resolve those against
                // the page/base URL, recursively pulling them in BEFORE the inline body's rules.
                process_css_text(
                    &src,
                    base,
                    &mut sheets,
                    &mut console,
                    &mut fetched,
                    &mut seen,
                );
            }
            StyleSource::External(url) => {
                fetch_css(&url, &mut sheets, &mut console, &mut fetched, &mut seen);
            }
        }
    }
    (sheets, console)
}

/// Collect ONLY inline `<style>` stylesheets, parsed directly — NO `<link>`/`@import` network
/// fetches. Used for PARTIAL frames during a streaming load so early paints never block on the
/// network: they show page structure plus inline-CSS styling, and the final frame (built with the
/// full [`collect_stylesheets`]) adds external CSS. Inline `@import`s are intentionally NOT followed
/// here (they'd fetch); the cascade still applies its UA stylesheet on top of these.
fn collect_inline_stylesheets(doc: &dom::Document, base: &str) -> Vec<css::Stylesheet> {
    fn walk(doc: &dom::Document, id: dom::NodeId, base: &str, out: &mut Vec<css::Stylesheet>) {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag == "style" {
                let mut src = String::new();
                for &child in &doc.get(id).children {
                    if let dom::NodeData::Text(t) = &doc.get(child).data {
                        src.push_str(t);
                    }
                }
                // Inline `<style>` resolves relative `url(...)` against the document base URL.
                out.push(css::parse_with_base(&src, base));
                return; // a <style>'s text body isn't markup
            }
        }
        for &child in &doc.get(id).children {
            walk(doc, child, base, out);
        }
    }
    let mut out = Vec::new();
    walk(doc, doc.root(), base, &mut out);
    out
}

/// Heuristic HTML sniff for responses with an absent/generic `content_type`: true if the parsed
/// document contains any of the structural html/head/body/`<!doctype html>`-derived elements (the
/// lenient parser synthesizes `<html>`/`<head>`/`<body>` for real markup, but a plain-text body
/// parses to bare text under the root with no such elements).
fn document_looks_like_html(doc: &dom::Document) -> bool {
    fn walk(doc: &dom::Document, id: dom::NodeId) -> bool {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            let t = e.tag.as_str();
            if t == "html" || t == "head" || t == "body" || t == "p" || t == "div" {
                return true;
            }
        }
        doc.get(id).children.iter().any(|&c| walk(doc, c))
    }
    walk(doc, doc.root())
}

/// Parse `text` (a CSS body fetched/found at URL `base_url`) and append its stylesheet to
/// `sheets`, but FIRST follow each top-level `@import`: resolve the specifier against `base_url`,
/// recursively fetch+process it, and include its rules before `text`'s own (CSS precedence:
/// imported styles come first / lower precedence, in import order). `fetched`/`seen` track the
/// global file count cap and dedup.
fn process_css_text(
    text: &str,
    base_url: &str,
    sheets: &mut Vec<css::Stylesheet>,
    console: &mut Vec<String>,
    fetched: &mut usize,
    seen: &mut std::collections::HashSet<String>,
) {
    for spec in css::extract_imports(text) {
        match resolve_url(base_url, &spec) {
            Some(abs) => fetch_css(&abs, sheets, console, fetched, seen),
            None => console.push(format!("[skipped @import (unresolvable): {spec}]")),
        }
    }
    sheets.push(css::parse_with_base(text, base_url));
}

/// Fetch the external CSS at absolute URL `url`, then process it (following its own `@import`s).
/// Dedups against `seen` and enforces the [`MAX_EXTERNAL_STYLESHEETS`] fetch cap. A failed fetch
/// is a console note, not a panic.
fn fetch_css(
    url: &str,
    sheets: &mut Vec<css::Stylesheet>,
    console: &mut Vec<String>,
    fetched: &mut usize,
    seen: &mut std::collections::HashSet<String>,
) {
    if !seen.insert(url.to_string()) {
        return; // already fetched (dedup / cycle guard)
    }
    if *fetched >= MAX_EXTERNAL_STYLESHEETS {
        console.push(format!(
            "[skipped stylesheet (limit {MAX_EXTERNAL_STYLESHEETS} reached): {url}]"
        ));
        return;
    }
    *fetched += 1;
    match net::fetch(url) {
        Ok(resp) => {
            let text = String::from_utf8_lossy(&resp.body).into_owned();
            // Resolve this file's own `@import`s relative to the URL it was fetched under.
            process_css_text(&text, url, sheets, console, fetched, seen);
        }
        Err(e) => console.push(format!("[failed to load stylesheet: {url} — {e}]")),
    }
}

/// Walk the DOM in document order collecting `<img>` elements with a resolvable `src`, then
/// fetch + decode each into a [`DecodedImage`] keyed by its DOM node. Caps the number fetched
/// ([`MAX_IMAGES`]) and skips oversized decodes ([`MAX_IMAGE_PIXELS`]). Decode/fetch failures
/// are skipped (with a console note) and never panic. `data:` URLs are decoded inline (base64
/// or percent-encoded); SVG payloads decode but don't raster (`image` has no SVG support).
fn collect_images(
    doc: &dom::Document,
    base: &str,
    console: &mut Vec<String>,
) -> HashMap<dom::NodeId, DecodedImage> {
    // Gather (node, absolute-url) pairs in document order.
    fn walk(
        doc: &dom::Document,
        id: dom::NodeId,
        base: &str,
        out: &mut Vec<(dom::NodeId, String)>,
    ) {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag.eq_ignore_ascii_case("img") {
                if let Some(src) = e.attrs.get("src") {
                    let src = src.trim();
                    // Keep `data:` URLs verbatim (decoded inline below); resolve the rest.
                    if src.starts_with("data:") {
                        out.push((id, src.to_string()));
                    } else if let Some(abs) = resolve_url(base, src) {
                        out.push((id, abs));
                    }
                }
            }
        }
        for &child in &doc.get(id).children {
            walk(doc, child, base, out);
        }
    }
    let mut targets = Vec::new();
    walk(doc, doc.root(), base, &mut targets);
    if targets.len() > MAX_IMAGES {
        for (_, url) in targets.drain(MAX_IMAGES..) {
            console.push(format!(
                "[skipped image (limit {MAX_IMAGES} reached): {url}]"
            ));
        }
    }

    // `data:` images decode inline (no I/O); network images are fetched concurrently across a
    // small pool of scoped threads, since they're independent and order doesn't matter.
    let (data_targets, net_targets): (Vec<_>, Vec<_>) = targets
        .into_iter()
        .partition(|(_, url)| url.starts_with("data:"));

    let mut results: Vec<(dom::NodeId, String, Result<DecodedImage, String>)> = Vec::new();
    for (node, url) in data_targets {
        let r = decode_data_url(&url)
            .ok_or_else(|| "malformed data: URL".to_string())
            .and_then(|b| decode_image(&b).ok_or_else(|| "decode failed".to_string()));
        results.push((node, url, r));
    }

    if !net_targets.is_empty() {
        let n_threads = net_targets.len().clamp(1, 8);
        let chunks: Vec<Vec<(dom::NodeId, String)>> = {
            let mut cs: Vec<Vec<_>> = (0..n_threads).map(|_| Vec::new()).collect();
            for (i, t) in net_targets.into_iter().enumerate() {
                cs[i % n_threads].push(t);
            }
            cs
        };
        std::thread::scope(|s| {
            let handles: Vec<_> = chunks
                .into_iter()
                .map(|chunk| {
                    s.spawn(move || {
                        chunk
                            .into_iter()
                            .map(|(node, url)| {
                                let r = net::fetch(&url).and_then(|resp| {
                                    decode_image(&resp.body)
                                        .ok_or_else(|| "decode failed".to_string())
                                });
                                (node, url, r)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            for h in handles {
                results.extend(h.join().unwrap_or_default());
            }
        });
    }

    let mut images = HashMap::new();
    for (node, url, r) in results {
        match r {
            Ok(img) => {
                images.insert(node, img);
            }
            Err(e) => {
                let label = if url.starts_with("data:") {
                    "data: image"
                } else {
                    &url
                };
                console.push(format!("[failed to load image: {label} — {e}]"));
            }
        }
    }
    images
}

/// Decode raster image bytes into straight-alpha RGBA8. Returns `None` on decode failure or if
/// the decoded image would exceed [`MAX_IMAGE_PIXELS`]. Never panics.
fn decode_image(bytes: &[u8]) -> Option<DecodedImage> {
    // The `image` crate has no JPEG XL decoder, so route `.jxl` bytes to `jxl-oxide` (pure Rust).
    if is_jxl(bytes) {
        return decode_jxl(bytes);
    }
    let dynimg = image::load_from_memory(bytes).ok()?;
    let w = dynimg.width();
    let h = dynimg.height();
    if (w as u64) * (h as u64) > MAX_IMAGE_PIXELS {
        return None;
    }
    let rgba = dynimg.to_rgba8();
    Some(DecodedImage {
        rgba: rgba.into_raw(),
        w,
        h,
    })
}

/// Whether `bytes` look like a JPEG XL stream: either the raw codestream marker (`FF 0A`) or the
/// ISOBMFF container's signature box (`00000000C 'JXL ' 0D 0A 87 0A`).
fn is_jxl(bytes: &[u8]) -> bool {
    const CONTAINER: [u8; 12] = [
        0x00, 0x00, 0x00, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A,
    ];
    bytes.starts_with(&[0xFF, 0x0A]) || bytes.starts_with(&CONTAINER)
}

/// Decode a JPEG XL image to straight-alpha RGBA8 via `jxl-oxide`. The decoder yields interleaved
/// `f32` samples in `[0, 1]` with 1 (gray), 2 (gray+alpha), 3 (RGB) or 4 (RGBA) channels; we expand
/// each to RGBA8. Returns `None` on a decode failure or if the image exceeds [`MAX_IMAGE_PIXELS`].
fn decode_jxl(bytes: &[u8]) -> Option<DecodedImage> {
    let image = jxl_oxide::JxlImage::builder().read(bytes).ok()?;
    let w = image.width();
    let h = image.height();
    if (w as u64) * (h as u64) > MAX_IMAGE_PIXELS {
        return None;
    }
    let render = image.render_frame(0).ok()?;
    let fb = render.image_all_channels();
    let channels = fb.channels();
    if channels == 0 {
        return None;
    }
    let buf = fb.buf();
    let px = (w as usize).checked_mul(h as usize)?;
    if buf.len() < px * channels {
        return None;
    }
    let to_u8 = |f: f32| (f.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    let mut rgba = Vec::with_capacity(px * 4);
    for i in 0..px {
        let base = i * channels;
        let (r, g, b, a) = match channels {
            1 => {
                let v = to_u8(buf[base]);
                (v, v, v, 255)
            }
            2 => {
                let v = to_u8(buf[base]);
                (v, v, v, to_u8(buf[base + 1]))
            }
            3 => (
                to_u8(buf[base]),
                to_u8(buf[base + 1]),
                to_u8(buf[base + 2]),
                255,
            ),
            _ => (
                to_u8(buf[base]),
                to_u8(buf[base + 1]),
                to_u8(buf[base + 2]),
                to_u8(buf[base + 3]),
            ),
        };
        rgba.extend_from_slice(&[r, g, b, a]);
    }
    Some(DecodedImage { rgba, w, h })
}

/// Decode a `data:[<mediatype>][;base64],<data>` URL into its raw bytes. Returns `None` if it
/// isn't a well-formed data URL. (SVG data URLs decode fine here but won't raster — `image`
/// has no SVG support — and are dropped at the `decode_image` step.)
fn decode_data_url(url: &str) -> Option<Vec<u8>> {
    let rest = url.strip_prefix("data:")?;
    let comma = rest.find(',')?;
    let meta = &rest[..comma];
    let payload = &rest[comma + 1..];
    if meta.split(';').any(|t| t.eq_ignore_ascii_case("base64")) {
        base64_decode(payload)
    } else {
        Some(percent_decode(payload))
    }
}

/// Minimal standard/URL-safe base64 decoder (ignores padding/whitespace). No external dep.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        })
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut buf, mut bits) = (0u32, 0u32);
    for &c in s.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        buf = (buf << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// Percent-decode bytes (`%HH`), passing other bytes through.
fn percent_decode(s: &str) -> Vec<u8> {
    fn hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Draw a single run at `(x, baseline)`. If `bold`, approximate bold by drawing each glyph
/// twice with a 1px horizontal offset ("faux bold"). `letter_spacing` px is added per character.
/// Returns the final pen x (end of the run), used to size text-decoration underlines.
#[allow(clippy::too_many_arguments)]
fn draw_run(
    fb: &mut Framebuffer,
    font: &dyn GlyphRasterizer,
    text: &str,
    x: f32,
    baseline_y: f32,
    px: f32,
    color: Color,
    bold: bool,
    letter_spacing: f32,
) -> f32 {
    let end = draw_text_spaced(fb, font, text, x, baseline_y, px, color, letter_spacing);
    if bold {
        draw_text_spaced(
            fb,
            font,
            text,
            x + 1.0,
            baseline_y,
            px,
            color,
            letter_spacing,
        );
    }
    end
}

/// One executable script slot in document order: an inline `<script>` body, or an external
/// `<script src>` whose `src` resolved to an absolute URL. Pure classification, no fetching.
#[derive(Debug, PartialEq, Eq)]
pub enum ScriptSource {
    Inline(String),
    External(String),
}

/// Walk the DOM in document order, classifying each *runnable* `<script>` element. Inline
/// scripts contribute their text body; `<script src>` contribute the resolved absolute URL.
/// Scripts with a non-JS `type` (e.g. `application/json`, `application/ld+json`) are omitted.
/// `<script type="module">` is also skipped here — modules are collected separately by
/// [`collect_module_entries`] and run (deferred) via [`run_modules`]. Pure: unit-testable
/// without network.
pub fn collect_script_sources(doc: &dom::Document, base: &str) -> Vec<ScriptSource> {
    fn is_js_type(ty: Option<&str>) -> bool {
        match ty {
            None => true,
            Some(t) => {
                let t = t.trim().to_ascii_lowercase();
                t.is_empty()
                    || t == "text/javascript"
                    || t == "application/javascript"
                    || t == "text/ecmascript"
                    || t == "application/ecmascript"
            }
        }
    }
    fn walk(doc: &dom::Document, id: dom::NodeId, base: &str, out: &mut Vec<ScriptSource>) {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag == "script" {
                if is_js_type(e.attrs.get("type").map(String::as_str)) {
                    if let Some(src) = e.attrs.get("src") {
                        if let Some(abs) = resolve_url(base, src) {
                            out.push(ScriptSource::External(abs));
                        }
                    } else {
                        // The HTML parser stores a script's body as a single Text child.
                        let mut source = String::new();
                        for &child in &doc.get(id).children {
                            if let dom::NodeData::Text(t) = &doc.get(child).data {
                                source.push_str(t);
                            }
                        }
                        out.push(ScriptSource::Inline(source));
                    }
                }
                // Don't descend into a script's children (its text body isn't markup).
                return;
            }
        }
        for &child in &doc.get(id).children {
            walk(doc, child, base, out);
        }
    }
    let mut out = Vec::new();
    walk(doc, doc.root(), base, &mut out);
    out
}

/// Maximum number of modules fetched per page's module graph (across all entries).
const MAX_MODULES: usize = 400;
/// Skip module sources larger than this (Vue's runtime is ~400 KiB; 16 MiB is generous).
const MAX_MODULE_BYTES: usize = 16 * 1024 * 1024;

/// One ES-module entry point in document order: an inline `<script type=module>` body (with the
/// page URL as its base), or an external `<script type=module src>` whose `src` resolved to an
/// absolute URL. Pure classification, no fetching.
#[derive(Debug, PartialEq, Eq)]
pub enum ModuleEntry {
    /// Inline module source. The base URL for resolving its imports is the page URL.
    Inline(String),
    /// External module URL (already resolved to an absolute `http(s)`/`file` URL).
    External(String),
}

/// Walk the DOM in document order, collecting `<script type="module">` elements (the ones
/// [`collect_script_sources`] deliberately skips). Inline modules contribute their text body;
/// external `<script type=module src>` contribute the resolved absolute URL. Pure: unit-testable
/// without network.
pub fn collect_module_entries(doc: &dom::Document, base: &str) -> Vec<ModuleEntry> {
    fn is_module_type(ty: Option<&str>) -> bool {
        matches!(ty, Some(t) if t.trim().eq_ignore_ascii_case("module"))
    }
    fn walk(doc: &dom::Document, id: dom::NodeId, base: &str, out: &mut Vec<ModuleEntry>) {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag == "script" {
                if is_module_type(e.attrs.get("type").map(String::as_str)) {
                    if let Some(src) = e.attrs.get("src") {
                        if let Some(abs) = resolve_url(base, src) {
                            out.push(ModuleEntry::External(abs));
                        }
                    } else {
                        let mut source = String::new();
                        for &child in &doc.get(id).children {
                            if let dom::NodeData::Text(t) = &doc.get(child).data {
                                source.push_str(t);
                            }
                        }
                        out.push(ModuleEntry::Inline(source));
                    }
                }
                return;
            }
        }
        for &child in &doc.get(id).children {
            walk(doc, child, base, out);
        }
    }
    let mut out = Vec::new();
    walk(doc, doc.root(), base, &mut out);
    out
}

/// One import/export specifier found in a module's source: the byte range of the *quoted* string
/// literal (including its quotes) plus the unquoted specifier text. Used to both resolve and
/// rewrite specifiers in place.
#[derive(Debug, PartialEq, Eq)]
struct SpecifierRef {
    /// Byte offset of the opening quote in the source.
    start: usize,
    /// Byte offset just past the closing quote.
    end: usize,
    /// The specifier string between the quotes (no quotes).
    spec: String,
}

/// Tolerantly scan `src` for static, string-literal module specifiers in `import`/`export`
/// statements and dynamic `import(...)` calls. Recognizes:
///   - `import ... from 'spec'` / `import ... from "spec"`
///   - `import 'spec'` (side-effect)
///   - `export ... from 'spec'` / `export * from 'spec'`
///   - `import('spec')` (dynamic, string-literal argument only)
///
/// This is a lexical scan, not a full parse: it skips line/block comments and string/template
/// literals so the keywords/quotes inside them aren't mistaken for imports, then looks for the
/// `from` / bare-import / `import(` patterns. Only static string literals are returned;
/// computed dynamic imports (`import(expr)`) are ignored. Never panics.
fn extract_specifiers(src: &str) -> Vec<SpecifierRef> {
    let b = src.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut i = 0usize;

    // Read a quoted string literal starting at the opening quote `b[i]`; returns (spec, end).
    fn read_string(b: &[u8], i: usize) -> Option<(String, usize)> {
        let quote = b[i];
        let mut j = i + 1;
        let mut s = Vec::new();
        while j < b.len() {
            let c = b[j];
            if c == b'\\' {
                // Keep escapes verbatim; specifiers rarely use them and we only need the URL form.
                if j + 1 < b.len() {
                    s.push(b[j + 1]);
                    j += 2;
                    continue;
                }
                return None;
            }
            if c == quote {
                return Some((String::from_utf8_lossy(&s).into_owned(), j + 1));
            }
            if c == b'\n' {
                return None; // unterminated single-line string
            }
            s.push(c);
            j += 1;
        }
        None
    }

    // Is the byte at `p` a JS identifier char (so we can require word boundaries around keywords)?
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';

    while i < n {
        let c = b[i];
        // Skip comments.
        if c == b'/' && i + 1 < n && b[i + 1] == b'/' {
            i += 2;
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        // Skip string / template literals (so their contents aren't scanned for keywords).
        if c == b'"' || c == b'\'' {
            match read_string(b, i) {
                Some((_, end)) => {
                    i = end;
                    continue;
                }
                None => {
                    i += 1;
                    continue;
                }
            }
        }
        if c == b'`' {
            // Template literal: skip to the matching backtick, honoring escapes. Nested `${}` may
            // contain backticks; we don't fully track them, but mismatches only cause us to miss a
            // specifier, never to misrewrite one.
            let mut j = i + 1;
            while j < n {
                if b[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if b[j] == b'`' {
                    break;
                }
                j += 1;
            }
            i = (j + 1).min(n);
            continue;
        }

        // Match `import` or `export` keyword at a word boundary.
        let is_import = b[i..].starts_with(b"import");
        let word = if is_import || b[i..].starts_with(b"export") {
            Some(6)
        } else {
            None
        };
        if let Some(kw_len) = word {
            let before_ok = i == 0 || !is_ident(b[i - 1]);
            let after = i + kw_len;
            let after_ok = after >= n || !is_ident(b[after]);
            if before_ok && after_ok {
                // Dynamic `import(...)`: skip whitespace after the keyword, expect `(`.
                if is_import {
                    let mut k = after;
                    while k < n && b[k].is_ascii_whitespace() {
                        k += 1;
                    }
                    if k < n && b[k] == b'(' {
                        k += 1;
                        while k < n && b[k].is_ascii_whitespace() {
                            k += 1;
                        }
                        if k < n && (b[k] == b'"' || b[k] == b'\'') {
                            if let Some((spec, end)) = read_string(b, k) {
                                out.push(SpecifierRef {
                                    start: k,
                                    end,
                                    spec,
                                });
                                i = end;
                                continue;
                            }
                        }
                        i = after;
                        continue;
                    }
                }
                // Static import/export. For a bare side-effect `import 'spec'`, the next non-space
                // token is the string itself. Otherwise the specifier follows a `from` keyword,
                // bounded by the statement terminator (`;`) or end of source.
                if is_import {
                    let mut p = after;
                    while p < n && b[p].is_ascii_whitespace() {
                        p += 1;
                    }
                    if p < n && (b[p] == b'"' || b[p] == b'\'') {
                        if let Some((spec, end)) = read_string(b, p) {
                            out.push(SpecifierRef {
                                start: p,
                                end,
                                spec,
                            });
                            i = end;
                            continue;
                        }
                    }
                }
                // Scan to a `from` keyword (bounded by the next `;`), then read the string after it.
                let stmt_end = b[after..]
                    .iter()
                    .position(|&c| c == b';')
                    .map(|off| after + off)
                    .unwrap_or(n);
                let mut k = after;
                let mut matched = false;
                while k < stmt_end {
                    if b[k..].starts_with(b"from")
                        && (k == 0 || !is_ident(b[k - 1]))
                        && (k + 4 >= n || !is_ident(b[k + 4]))
                    {
                        let mut p = k + 4;
                        while p < stmt_end && b[p].is_ascii_whitespace() {
                            p += 1;
                        }
                        if p < stmt_end && (b[p] == b'"' || b[p] == b'\'') {
                            if let Some((spec, end)) = read_string(b, p) {
                                out.push(SpecifierRef {
                                    start: p,
                                    end,
                                    spec,
                                });
                                i = end;
                                matched = true;
                                break;
                            }
                        }
                    }
                    k += 1;
                }
                if !matched {
                    i = after;
                }
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Classify a specifier for module resolution.
fn is_bare_specifier(spec: &str) -> bool {
    let s = spec.trim();
    if s.starts_with("./") || s.starts_with("../") || s.starts_with('/') {
        return false;
    }
    // A scheme like `http:`/`https:`/`file:` makes it absolute, not bare.
    !matches!(
        s.split_once(':'),
        Some((scheme, _)) if scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
            && scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
    )
}

/// Build the page's ES-module graph by fetching the entries and every transitively-imported
/// module, rewriting each module's import/export specifiers to the canonical absolute URL it was
/// fetched under. Returns the entry canonical URLs (document order), a `url -> rewritten source`
/// map, and console notes for skipped bare imports / failed loads.
///
/// `inline_counter` produces a synthetic unique URL for each inline `<script type=module>` so the
/// loader can key it; its imports resolve against the page URL.
pub fn collect_module_graph(
    doc: &dom::Document,
    page_url: &str,
) -> (Vec<String>, HashMap<String, String>, Vec<String>) {
    let entries_raw = collect_module_entries(doc, page_url);
    if entries_raw.is_empty() {
        return (Vec::new(), HashMap::new(), Vec::new());
    }

    let mut sources: HashMap<String, String> = HashMap::new();
    let mut notes: Vec<String> = Vec::new();
    let mut entry_urls: Vec<String> = Vec::new();
    // Work queue of (canonical url, base url for resolving its imports) modules to process.
    let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    // Raw (un-rewritten) sources fetched so far, keyed by canonical url, with the base for imports.
    let mut raw: HashMap<String, String> = HashMap::new();

    let mut inline_idx = 0usize;
    for entry in entries_raw {
        match entry {
            ModuleEntry::Inline(src) => {
                let url = format!("{page_url}#inline-module-{inline_idx}");
                inline_idx += 1;
                entry_urls.push(url.clone());
                raw.insert(url.clone(), src);
                queue.push_back(url);
            }
            ModuleEntry::External(url) => {
                if entry_urls.contains(&url) || raw.contains_key(&url) {
                    if !entry_urls.contains(&url) {
                        entry_urls.push(url);
                    }
                    continue;
                }
                entry_urls.push(url.clone());
                queue.push_back(url);
            }
        }
    }

    // BFS the graph level-by-level, fetching each level's modules CONCURRENTLY (a Vue app pulls
    // 200+ modules — sequential fetch dominated load time). `seen` dedups everything ever queued.
    let mut seen: std::collections::HashSet<String> = entry_urls.iter().cloned().collect();
    let mut frontier: Vec<String> = queue.into_iter().collect();

    while !frontier.is_empty() {
        // Cap the total module count, trimming this level's overflow with a note.
        let remaining = MAX_MODULES.saturating_sub(sources.len());
        if remaining == 0 {
            for u in &frontier {
                notes.push(format!(
                    "[skipped module (limit {MAX_MODULES} reached): {u}]"
                ));
            }
            break;
        }
        if frontier.len() > remaining {
            for u in frontier.split_off(remaining) {
                notes.push(format!(
                    "[skipped module (limit {MAX_MODULES} reached): {u}]"
                ));
            }
        }

        // Separate inline sources (already in `raw`) from network URLs to fetch.
        let mut bodies: Vec<(String, Result<String, String>)> = Vec::new();
        let mut net_urls: Vec<String> = Vec::new();
        for url in frontier.drain(..) {
            if let Some(src) = raw.remove(&url) {
                bodies.push((url, Ok(src)));
            } else if !url.contains("#inline-module-") {
                net_urls.push(url);
            }
        }

        // Fetch this level concurrently across a small scoped thread pool.
        if !net_urls.is_empty() {
            let n = net_urls.len().clamp(1, 8);
            let mut chunks: Vec<Vec<String>> = (0..n).map(|_| Vec::new()).collect();
            for (i, u) in net_urls.into_iter().enumerate() {
                chunks[i % n].push(u);
            }
            std::thread::scope(|s| {
                let handles: Vec<_> = chunks
                    .into_iter()
                    .map(|chunk| {
                        s.spawn(move || {
                            chunk
                                .into_iter()
                                .map(|u| {
                                    let r = match net::fetch(&u) {
                                        Ok(resp) if resp.body.len() > MAX_MODULE_BYTES => {
                                            Err(format!(
                                                "[skipped large module: {} ({} bytes)]",
                                                u,
                                                resp.body.len()
                                            ))
                                        }
                                        Ok(resp) => {
                                            Ok(String::from_utf8_lossy(&resp.body).into_owned())
                                        }
                                        Err(e) => {
                                            Err(format!("[failed to load module: {u} — {e}]"))
                                        }
                                    };
                                    (u, r)
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                for h in handles {
                    bodies.extend(h.join().unwrap_or_default());
                }
            });
        }

        // Process each fetched module: rewrite specifiers to canonical URLs, discover next level.
        let mut next: Vec<String> = Vec::new();
        for (url, body_res) in bodies {
            let body = match body_res {
                Ok(b) => b,
                Err(note) => {
                    notes.push(note);
                    continue;
                }
            };
            // Imports resolve against the page URL for inline entries, else the module's own URL.
            let base = if url.contains("#inline-module-") {
                page_url.to_string()
            } else {
                url.clone()
            };
            let specs = extract_specifiers(&body);
            let mut replacements: Vec<(usize, usize, String)> = Vec::new();
            for sp in &specs {
                if is_bare_specifier(&sp.spec) {
                    notes.push(format!("[skipped bare import: {}]", sp.spec));
                    continue;
                }
                let resolved = match url::Url::parse(&base)
                    .ok()
                    .and_then(|b| b.join(sp.spec.trim()).ok())
                {
                    Some(u) => u.to_string(),
                    None => {
                        notes.push(format!(
                            "[failed to resolve import: {} (in {url})]",
                            sp.spec
                        ));
                        continue;
                    }
                };
                let scheme = resolved.split(':').next().unwrap_or("");
                if !matches!(scheme, "http" | "https" | "file") {
                    notes.push(format!("[skipped non-loadable import: {}]", sp.spec));
                    continue;
                }
                let quote = &body[sp.start..sp.start + 1];
                replacements.push((sp.start, sp.end, format!("{quote}{resolved}{quote}")));
                if !seen.contains(&resolved) {
                    seen.insert(resolved.clone());
                    next.push(resolved);
                }
            }
            replacements.sort_by_key(|r| std::cmp::Reverse(r.0));
            let mut rewritten = body;
            for (start, end, rep) in replacements {
                rewritten.replace_range(start..end, &rep);
            }
            sources.insert(url, rewritten);
        }
        frontier = next;
    }

    (entry_urls, sources, notes)
}

/// Run the page's ES modules (deferred — after classic scripts). Builds the module graph
/// (fetch + rewrite via [`collect_module_graph`]) and executes it through [`js::run_modules`] so
/// modules share the same DOM-wired `document`/`window` classic scripts use. Returns the mutated
/// document plus console/error/note lines (errors prefixed `⚠`).
pub fn run_modules(doc: dom::Document, page_url: &str) -> (dom::Document, Vec<String>) {
    let (entries, sources, notes) = collect_module_graph(&doc, page_url);
    if entries.is_empty() {
        return (doc, notes);
    }
    // On-demand fetcher for dynamic imports of modules not in the pre-fetched static graph.
    // Called only on the JS isolate's own worker thread, so blocking `net::fetch` is fine here.
    let fetcher: Box<dyn Fn(&str) -> Option<(String, String)> + Send> = Box::new(|u: &str| {
        net::fetch(u).ok().map(|r| {
            (
                String::from_utf8_lossy(&r.body).into_owned(),
                r.content_type,
            )
        })
    });
    let request_fetcher = build_request_fetcher();
    let (doc, results) = js::run_modules(doc, page_url, entries, sources, fetcher, request_fetcher);
    let mut out = notes;
    for result in results {
        out.extend(result.console);
        if let Some(err) = result.error {
            out.push(format!("⚠ {err}"));
        }
    }
    (doc, out)
}

/// Collect the page's scripts in document order — inline `<script>` bodies and external
/// `<script src>` (fetched against `base`) — and run them all on a single shared [`js`]
/// context (so later scripts see earlier globals AND each other's DOM mutations). Returns the
/// mutated document plus all captured console lines and any error lines (prefixed `⚠`).
/// Failed/over-limit/too-large external fetches contribute a `[…]` note in document order.
/// External fetches are sequential (classic blocking-script order); correctness over speed.
///
/// Takes the document by value and returns it: the JS path needs to *own* the tree to mutate
/// it (e.g. `el.textContent = "..."`), so the returned, possibly-mutated document is what the
/// caller should store and render.
pub fn run_scripts(doc: dom::Document, base: &str) -> (dom::Document, Vec<String>) {
    let classified = collect_script_sources(&doc, base);
    if classified.is_empty() {
        return (doc, Vec::new());
    }

    // Per-slot outcome in document order: either an executed source (indexed into `sources`)
    // or a pre-formatted skip/failure note to emit verbatim.
    enum Slot {
        Source(usize),
        Note(String),
    }
    let mut slots = Vec::new();
    let mut sources: Vec<String> = Vec::new();
    let mut fetched = 0usize;
    for item in classified {
        match item {
            ScriptSource::Inline(src) => {
                if src.len() > MAX_SCRIPT_BYTES {
                    slots.push(Slot::Note(format!(
                        "[skipped large script: {} bytes]",
                        src.len()
                    )));
                } else {
                    slots.push(Slot::Source(sources.len()));
                    sources.push(src);
                }
            }
            ScriptSource::External(url) => {
                if fetched >= MAX_EXTERNAL_SCRIPTS {
                    slots.push(Slot::Note(format!(
                        "[skipped script (limit {MAX_EXTERNAL_SCRIPTS} reached): {url}]"
                    )));
                    continue;
                }
                fetched += 1;
                match net::fetch(&url) {
                    Ok(resp) if resp.body.len() > MAX_SCRIPT_BYTES => {
                        slots.push(Slot::Note(format!(
                            "[skipped large script: {} ({} bytes)]",
                            url,
                            resp.body.len()
                        )))
                    }
                    Ok(resp) => {
                        slots.push(Slot::Source(sources.len()));
                        sources.push(String::from_utf8_lossy(&resp.body).into_owned());
                    }
                    Err(e) => {
                        slots.push(Slot::Note(format!("[failed to load script: {url} — {e}]")))
                    }
                }
            }
        }
    }

    // Execute all sources on one DOM-aware context (off-thread, large stack) in document order
    // so later scripts see earlier globals and DOM mutations. Returns the mutated document.
    let (doc, results) = if sources.is_empty() {
        (doc, Vec::new())
    } else {
        js::run_with_dom(doc, sources, base)
    };

    let mut out = Vec::new();
    for slot in slots {
        match slot {
            Slot::Source(i) => {
                if let Some(result) = results.get(i) {
                    out.extend(result.console.iter().cloned());
                    if let Some(err) = &result.error {
                        out.push(format!("⚠ {err}"));
                    }
                }
            }
            Slot::Note(note) => out.push(note),
        }
    }
    (doc, out)
}

/// Gather the page's classic script sources (inline + external `<script src>`) and its ES-module
/// graph, then start a persistent [`js::Session`] that runs them and stays alive for interactivity.
/// Returns the session (None if the page has no scripts/modules), the initial DOM snapshot, and
/// console/error/note lines. Mirrors the gathering in [`run_scripts`]/[`run_modules`] but hands the
/// work to a long-lived runtime instead of a run-once worker.
#[allow(clippy::type_complexity)]
fn start_session(
    doc: dom::Document,
    base: &str,
    initial_rects: Option<(
        Vec<(usize, f32, f32, f32, f32)>,
        Vec<(usize, f32, f32)>,
        Vec<(usize, f32, f32, f32, f32)>,
        Vec<(usize, f32, f32, f32, f32)>,
        f32,
        f32,
    )>,
) -> (Option<js::Session>, dom::Document, Vec<String>) {
    let mut notes: Vec<String> = Vec::new();

    // Classic scripts, in document order.
    let mut classic: Vec<String> = Vec::new();
    let mut fetched = 0usize;
    for item in collect_script_sources(&doc, base) {
        match item {
            ScriptSource::Inline(src) => {
                if src.len() > MAX_SCRIPT_BYTES {
                    notes.push(format!("[skipped large script: {} bytes]", src.len()));
                } else {
                    classic.push(src);
                }
            }
            ScriptSource::External(url) => {
                if fetched >= MAX_EXTERNAL_SCRIPTS {
                    notes.push(format!(
                        "[skipped script (limit {MAX_EXTERNAL_SCRIPTS} reached): {url}]"
                    ));
                    continue;
                }
                fetched += 1;
                match net::fetch(&url) {
                    Ok(resp) if resp.body.len() > MAX_SCRIPT_BYTES => notes.push(format!(
                        "[skipped large script: {} ({} bytes)]",
                        url,
                        resp.body.len()
                    )),
                    Ok(resp) => classic.push(String::from_utf8_lossy(&resp.body).into_owned()),
                    Err(e) => notes.push(format!("[failed to load script: {url} — {e}]")),
                }
            }
        }
    }

    // ES module graph.
    let (entries, module_sources, mod_notes) = collect_module_graph(&doc, base);
    notes.extend(mod_notes);

    // Note: we previously short-circuited to `(None, …)` when a page had no scripts. That left
    // script-less pages with no live JS runtime, so `console_eval` / WebDriver script execution
    // (and any later runtime DOM query) returned "(no live page)". A real browser always exposes a
    // scriptable document, so we now start a session even with zero author scripts — the cost is one
    // idle isolate, and it makes `Engine::console_eval` work on every loaded HTML page.

    let fetcher: Box<dyn Fn(&str) -> Option<(String, String)> + Send> = Box::new(|u: &str| {
        net::fetch(u).ok().map(|r| {
            (
                String::from_utf8_lossy(&r.body).into_owned(),
                r.content_type,
            )
        })
    });
    let request_fetcher = build_request_fetcher();
    let ws_connector = build_ws_connector();
    let (session, snapshot, results) = js::Session::new(
        doc,
        classic,
        entries,
        module_sources,
        base,
        fetcher,
        request_fetcher,
        ws_connector,
        initial_rects,
    );
    for result in results {
        notes.extend(result.console);
        if let Some(err) = result.error {
            notes.push(format!("⚠ {err}"));
        }
    }
    (Some(session), snapshot, notes)
}

fn collect_text(doc: &dom::Document, id: dom::NodeId, out: &mut String) {
    match &doc.get(id).data {
        dom::NodeData::Text(t) => out.push_str(t),
        dom::NodeData::Element(e) => {
            if SKIP_SUBTREE.contains(&e.tag.as_str()) {
                return;
            }
            let block = BLOCK_TAGS.contains(&e.tag.as_str());
            if block {
                out.push('\n');
            }
            for &child in &doc.get(id).children {
                collect_text(doc, child, out);
            }
            if block {
                out.push('\n');
            }
        }
        dom::NodeData::Document | dom::NodeData::DocumentFragment => {
            for &child in &doc.get(id).children {
                collect_text(doc, child, out);
            }
        }
        dom::NodeData::Comment(_)
        | dom::NodeData::DocumentType(_)
        | dom::NodeData::ProcessingInstruction(_) => {}
    }
}

/// Collapse runs of ASCII whitespace into single spaces, but preserve `\n` (paragraph
/// breaks) introduced by block elements. Leading/trailing space on each line is trimmed.
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    // First, normalize so each newline is a hard break and other whitespace collapses.
    let mut pending_space = false;
    let mut at_line_start = true;
    for ch in s.chars() {
        if ch == '\n' {
            // Trim trailing space already handled by pending_space reset.
            if !out.ends_with('\n') && !out.is_empty() {
                out.push('\n');
            }
            pending_space = false;
            at_line_start = true;
        } else if ch.is_ascii_whitespace() {
            pending_space = true;
        } else {
            if pending_space && !at_line_start {
                out.push(' ');
            }
            pending_space = false;
            at_line_start = false;
            out.push(ch);
        }
    }
    // Trim a trailing newline.
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Greedy word-wrap painter. Splits `text` on `\n` into paragraphs, then on spaces into
/// words, advancing `*baseline` per line. Stops painting once we run past `max_y`.
#[allow(clippy::too_many_arguments)]
fn draw_wrapped_text(
    fb: &mut Framebuffer,
    font: &dyn GlyphRasterizer,
    text: &str,
    left: f32,
    baseline: &mut f32,
    px: f32,
    line_h: f32,
    max_x: f32,
    max_y: f32,
    color: Color,
) {
    let space_w = font.advance(' ', px);
    for paragraph in text.split('\n') {
        let mut pen = left;
        let mut wrote_word = false;
        for word in paragraph.split(' ').filter(|w| !w.is_empty()) {
            let w_width = measure_text(font, word, px);
            // Wrap if this word would overflow and we've already placed something.
            if wrote_word && pen + space_w + w_width > max_x {
                *baseline += line_h;
                pen = left;
                wrote_word = false;
            }
            if *baseline > max_y {
                return;
            }
            if wrote_word {
                pen += space_w;
            }
            draw_text(fb, font, word, pen, *baseline, px, color);
            pen += w_width;
            wrote_word = true;
        }
        // End of paragraph: advance to next line.
        *baseline += line_h;
        if *baseline > max_y {
            return;
        }
    }
}

/// Sum of glyph advances for `text` at size `px`.
fn measure_text(font: &dyn GlyphRasterizer, text: &str, px: f32) -> f32 {
    text.chars().map(|ch| font.advance(ch, px)).sum()
}

/// Paint a console panel along the bottom of the framebuffer: a divider, a "console" label,
/// and the captured lines (in order). `panel_top` is the y where the page-text region ended.
/// No longer called: the console now lives in the Swift devtools panel. Kept for reference.
#[allow(dead_code)]
fn draw_console_panel(
    fb: &mut Framebuffer,
    font: &dyn GlyphRasterizer,
    lines: &[String],
    scale: f32,
    dw: u32,
    dh: u32,
    panel_top: f32,
) {
    let top = panel_top.max(0.0) as i32;

    // Panel background (slightly darker than the gradient) and a top divider line.
    fb.fill_rect(
        Rect {
            x: 0,
            y: top,
            w: dw as i32,
            h: (dh as i32 - top).max(0),
        },
        Color::rgb(14, 15, 20),
    );
    fb.fill_rect(
        Rect {
            x: 0,
            y: top,
            w: dw as i32,
            h: (2.0 * scale).max(1.0) as i32,
        },
        Color::rgb(60, 120, 160),
    );

    let left = 12.0 * scale;
    let label_px = 12.0 * scale;
    let line_px = 12.0 * scale;
    let line_h = line_px * 1.35;

    // "console" label just under the divider.
    let mut baseline = top as f32 + label_px + 6.0 * scale;
    draw_text(
        fb,
        font,
        "console",
        left,
        baseline,
        label_px,
        Color::rgb(120, 200, 255),
    );
    baseline += line_h;

    let max_y = dh as f32;
    let max_x = dw as f32 - left;
    for line in lines {
        if baseline > max_y {
            break;
        }
        // Errors (prefixed ⚠) get a warning color; normal logs are light grey.
        let color = if line.starts_with('⚠') {
            Color::rgb(255, 170, 120)
        } else {
            Color::rgb(210, 215, 225)
        };
        // Wrap each console line so long output doesn't run off the right edge.
        let mut line_baseline = baseline;
        draw_wrapped_text(
            fb,
            font,
            line,
            left,
            &mut line_baseline,
            line_px,
            line_h,
            max_x,
            max_y,
            color,
        );
        // Advance past however many wrapped rows this line consumed (at least one).
        baseline = line_baseline.max(baseline + line_h);
    }
}

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
        // Convert to device pixels (inverse of the layout->device mapping in render): with
        // scale 1.0 and scroll 0, device = layout + (left=16, header_h=8).
        let left = 16.0 * e.scale;
        let header_h = 8.0 * e.scale;
        let dx = lx + left;
        let dy = ly + (header_h - e.scroll_y);

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
            fn text_width(&self, text: &str, px: f32, bold: bool) -> f32 {
                let mut w = text.chars().count() as f32 * px * 0.5;
                if bold {
                    w += text.chars().count() as f32;
                }
                w
            }
            fn line_height(&self, px: f32) -> f32 {
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
        paint_box(
            &mut fb,
            &NoFont,
            &root,
            16.0,
            28.0,
            28.0,
            300.0,
            &images,
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
            fn text_width(&self, t: &str, px: f32, _b: bool) -> f32 {
                t.chars().count() as f32 * px * 0.5
            }
            fn line_height(&self, px: f32) -> f32 {
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
            paint_box(
                &mut fb,
                &NoFont,
                &root,
                0.0,
                0.0,
                0.0,
                200.0,
                &imgs,
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
        fn text_width(&self, t: &str, px: f32, _b: bool) -> f32 {
            t.chars().count() as f32 * px * 0.5
        }
        fn line_height(&self, px: f32) -> f32 {
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
        paint_box(
            &mut fb,
            &NF,
            &root,
            0.0,
            0.0,
            0.0,
            h as f32,
            &imgs,
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
            let m = FontMeasurer { font: &font };
            let plain = m.text_width("abc", 16.0, false);
            let bold = m.text_width("abc", 16.0, true);
            assert!(bold > plain, "bold {bold} should exceed plain {plain}");
            assert_eq!(m.line_height(10.0), 13.0);
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
            fn text_width(&self, t: &str, px: f32, _b: bool) -> f32 {
                t.chars().count() as f32 * px * 0.5
            }
            fn line_height(&self, px: f32) -> f32 {
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
