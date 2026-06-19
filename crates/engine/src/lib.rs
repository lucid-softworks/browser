//! The browser engine: owns the pipeline state and produces a painted framebuffer.
//!
//! Phase 0/1 scope: fetch a URL (via `net`), remember the result, and paint a status
//! screen — a computed gradient plus real text rendered by our compositor. The full
//! parse → style → layout → paint pipeline lands in later phases; the function boundaries
//! (`html::parse`, `style`, `layout`) already exist as stubs so wiring them in is additive.

mod font;

use std::collections::HashMap;

use font::SystemFont;
use paint::{Color, Framebuffer, GlyphRasterizer, Rect};

/// A decoded raster image ready to blit: straight-alpha RGBA8 pixels plus dimensions.
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
    Failed { url: String, error: String },
}

/// Cached cascade+layout result, reused across renders when only the scroll offset changes.
/// Invalidated on navigation (`load_url`) and when the device viewport size changes.
struct LayoutCache {
    dw: u32,
    dh: u32,
    root: layout::LayoutBox,
    content_h: f32,
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
            layout_cache: None,
            framebuffer: None,
            session: None,
            focused_node: None,
            focus_value: None,
            hovered_node: None,
        }
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

    /// Fetch `url` and remember the outcome. Returns 0 on success, negative on error.
    pub fn load_url(&mut self, url: &str) -> i32 {
        self.scroll_y = 0.0; // new navigation starts at the top
        self.layout_cache = None; // invalidate cached layout for the previous page
        self.focused_node = None; // a new page has no focused field
        self.focus_value = None;
        self.hovered_node = None; // and nothing is hovered
        match net::fetch(url) {
            Ok(resp) => {
                // Parse HTML responses into a DOM; other content types just record metadata.
                let doc = if resp.content_type.to_ascii_lowercase().contains("html") {
                    Some(html::parse(&String::from_utf8_lossy(&resp.body)))
                } else {
                    None
                };

                // Determine the page's base URL for resolving relative sub-resources: the
                // response's final URL, overridden by a `<base href>` element if present.
                let base = match &doc {
                    Some(d) => base_url(d, &resp.final_url),
                    None => resp.final_url.clone(),
                };

                // Start a persistent JS runtime: runs the page's classic scripts + ES modules and
                // stays alive so event handlers/timers keep working (interactivity). Returns the
                // initial DOM snapshot. Replaces the old run-once-and-drop path.
                let mut console: Vec<String> = Vec::new();
                self.session = None; // drop the previous page's runtime (stops its thread)
                let doc = match doc {
                    Some(d) => {
                        let (session, mut snapshot, sess_console) = start_session(d, &base);
                        console.extend(sess_console);
                        // Page JS can leave stale/garbage node ids in the tree; drop any that
                        // point outside the arena so layout/paint can't hit an out-of-bounds id.
                        snapshot.prune_invalid();
                        self.session = session;
                        Some(snapshot)
                    }
                    None => None,
                };

                // Collect stylesheets AFTER scripts/modules run, so CSS injected at runtime
                // (SPA frameworks add component `<style>` tags and fetch+inject CSS, e.g. Vue)
                // is included in the cascade, not just the static `<style>`/`<link>` from parse.
                let styles = match &doc {
                    Some(d) => {
                        let (s, style_console) = collect_stylesheets(d, &base);
                        console.extend(style_console);
                        s
                    }
                    None => Vec::new(),
                };

                // Fetch + decode `<img>` images (after scripts, so script-inserted images and
                // mutated `src` attributes are seen). Reset on every navigation.
                let images = match &doc {
                    Some(d) => collect_images(d, &base, &mut console),
                    None => HashMap::new(),
                };

                self.state = LoadState::Loaded {
                    url: resp.final_url,
                    doc,
                    styles,
                    console,
                    images,
                };
                0
            }
            Err(e) => {
                self.state = LoadState::Failed { url: url.to_string(), error: e };
                -1
            }
        }
    }

    /// Recompute the cascade + layout for the current viewport into `layout_cache`, unless a
    /// cached tree for this exact device size is already present. This is the expensive part of
    /// rendering; keeping it out of the scroll path makes scrolling cheap (paint-only).
    fn ensure_layout(&mut self, dw: u32, dh: u32, header_h: f32) {
        // Feed the real logical viewport + scale to the cascade so @media (width/height/resolution),
        // @container, and vw/vh units evaluate against the true window — and, since this runs on
        // every viewport change, they re-evaluate on resize.
        style::set_viewport_metrics(self.vp_w as f32, self.vp_h as f32, self.scale);
        if matches!(&self.layout_cache, Some(c) if c.dw == dw && c.dh == dh) {
            return;
        }
        // Compute into owned values first so the `&self.state` borrow ends before we assign.
        let computed = if let (Some(font), LoadState::Loaded { doc: Some(d), styles, console, images, .. }) =
            (self.font.as_ref(), &self.state)
        {
            let page_max_y = if console.is_empty() { dh as f32 } else { (dh as f32 * 0.65).floor() };
            let vw = (dw as f32).max(1.0);
            let vh = (page_max_y - header_h).max(1.0);
            let measurer = FontMeasurer { font };
            let intrinsic_sizes: HashMap<dom::NodeId, (f32, f32)> = images
                .iter()
                .map(|(&id, img)| (id, (img.w as f32, img.h as f32)))
                .collect();
            let computed = style::cascade(d, styles);
            let root =
                layout::layout_document(d, &computed, vw, vh, &measurer, &intrinsic_sizes, self.focused_node);
            let content_h = root.dimensions.margin_box().height;
            Some((root, content_h))
        } else {
            None
        };
        self.layout_cache = computed.map(|(root, content_h)| LayoutCache { dw, dh, root, content_h });
    }

    /// Paint the current state into a fresh framebuffer and return a reference to it.
    pub fn render(&mut self) -> &Framebuffer {
        let dw = ((self.vp_w as f32) * self.scale).round().max(1.0) as u32;
        let dh = ((self.vp_h as f32) * self.scale).round().max(1.0) as u32;
        // No engine inset: page paints flush at (0,0); margin/padding come from CSS.
        let header_h = 0.0;

        // Expensive: cascade + layout (cached across scrolls / repeated renders at this size).
        self.ensure_layout(dw, dh, header_h);

        let mut fb = Framebuffer::new(dw, dh);
        let mut scroll_y = self.scroll_y;

        paint_gradient(&mut fb);

        let px = 16.0 * self.scale;
        if let Some(font) = self.font.as_ref() {
            match &self.state {
                LoadState::Empty => {
                    draw_text(&mut fb, font, "browser — phase 2", 12.0 * self.scale,
                              19.0 * self.scale, 13.0 * self.scale, Color::rgb(120, 200, 255));
                    draw_text(&mut fb, font, "Enter a URL and press Go.",
                              12.0 * self.scale, 60.0 * self.scale, px, Color::WHITE);
                }
                LoadState::Loaded { url, doc, console, images, .. } => {
                    let left = 0.0;
                    let page_max_y = if console.is_empty() {
                        dh as f32
                    } else {
                        (dh as f32 * 0.65).floor()
                    };
                    let viewport_height = (page_max_y - header_h).max(1.0);

                    if let Some(cache) = &self.layout_cache {
                        // Scroll just re-paints the cached layout at a new offset.
                        let max_scroll = (cache.content_h - viewport_height).max(0.0);
                        scroll_y = scroll_y.min(max_scroll);
                        paint_box(
                            &mut fb, font, &cache.root, left, header_h - scroll_y, header_h,
                            page_max_y, images,
                        );
                    } else if doc.is_none() {
                        draw_text(
                            &mut fb, font, &format!("(non-HTML content: {})", url),
                            left, header_h + px * 1.4, px, Color::WHITE,
                        );
                    }

                    if !console.is_empty() {
                        draw_console_panel(
                            &mut fb, font, console, self.scale, dw, dh, page_max_y,
                        );
                    }
                }
                LoadState::Failed { url, error } => {
                    draw_text(&mut fb, font, "browser — phase 2", 12.0 * self.scale,
                              19.0 * self.scale, 13.0 * self.scale, Color::rgb(120, 200, 255));
                    let baseline = 60.0 * self.scale;
                    draw_text(&mut fb, font, &format!("Failed: {url}"),
                              16.0 * self.scale, baseline, px, Color::rgb(255, 120, 120));
                    draw_text(&mut fb, font, error, 16.0 * self.scale,
                              baseline + px * 1.4, px, Color::rgb(255, 180, 180));
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
            LoadState::Loaded { doc: Some(d), url, .. } => (d, url),
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
                    if self.focus_value.is_some() && self.focus_value.as_deref() != cur_val.as_deref() {
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

        // Submit: a click on a submit button (<input type=submit>, <button type=submit>, or a
        // <button> with no type) inside a form fires `submit` on the nearest ancestor <form>.
        if let Some(form) = submit_target_form(&snapshot, node) {
            let (s, c) = session.fire_event(form.0, "submit");
            snapshot = s;
            snapshot.prune_invalid();
            console.extend(c);
        }

        if let LoadState::Loaded { doc, console: c, .. } = &mut self.state {
            *doc = Some(snapshot);
            c.extend(console);
            self.layout_cache = None; // DOM may have changed → re-cascade/layout/paint
            true
        } else {
            false
        }
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
        if let Some(snap) = snapshot {
            if let LoadState::Loaded { doc, console: c, .. } = &mut self.state {
                *doc = Some(snap);
                c.extend(console);
                self.layout_cache = None;
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

        if let LoadState::Loaded { doc, console: c, .. } = &mut self.state {
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
        let session = match &self.session {
            Some(s) => s,
            None => return false,
        };
        // `None` => nothing was due this tick; cheap no-op (no snapshot clone, no re-render).
        let (mut snapshot, console) = match session.tick() {
            Some(r) => r,
            None => return false,
        };
        snapshot.prune_invalid();
        if let LoadState::Loaded { doc, console: c, .. } = &mut self.state {
            *doc = Some(snapshot);
            c.extend(console);
            self.layout_cache = None;
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
fn paint_gradient(fb: &mut Framebuffer) {
    let h = fb.height.max(1);
    for y in 0..fb.height {
        let t = y as f32 / h as f32;
        let c = Color::rgb(
            (18.0 + t * 10.0) as u8,
            (20.0 + t * 14.0) as u8,
            (28.0 + t * 26.0) as u8,
        );
        fb.fill_rect(Rect { x: 0, y: y as i32, w: fb.width as i32, h: 1 }, c);
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
    let ty = el.attrs.get("type").map(|s| s.trim().to_ascii_lowercase()).unwrap_or_default();
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
                let ty =
                    e.attrs.get("type").map(|s| s.trim().to_ascii_lowercase()).unwrap_or_default();
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
            let ty =
                e.attrs.get("type").map(|s| s.trim().to_ascii_lowercase()).unwrap_or_default();
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
) {
    paint_box_opacity(fb, font, b, ox, oy, clip_top, clip_bottom, images, 1.0);
}

/// Scale a u8 alpha by an effective opacity in 0.0..=1.0.
fn scale_alpha(a: u8, opacity: f32) -> u8 {
    ((a as f32) * opacity.clamp(0.0, 1.0)).round().clamp(0.0, 255.0) as u8
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
    ox: f32,
    oy: f32,
    clip_top: f32,
    clip_bottom: f32,
    images: &HashMap<dom::NodeId, DecodedImage>,
    parent_opacity: f32,
) {
    // This box's opacity multiplies into the inherited (effective) opacity for itself + subtree.
    let opacity = parent_opacity * b.style.opacity.clamp(0.0, 1.0);

    let border = b.dimensions.border_box();
    let content = b.dimensions.content;
    let radius = b.style.border_radius;
    // Translate the box's vertical extent into device space for clipping.
    let top = border.y.min(content.y) + oy;
    let bottom = (border.y + border.height).max(content.y + content.height) + oy;
    // Fully outside the visible band: skip this box (children may still be inside, so only
    // skip when even the box's lower edge is above the band, or its top is below it).
    let offscreen = bottom < clip_top || top >= clip_bottom;

    if !offscreen && opacity > 0.0 {
        // (a) Background fills the border box (rounded if border-radius is set).
        if let Some((r, g, bl)) = b.style.background_color {
            let c = Color { r, g, b: bl, a: scale_alpha(255, opacity) };
            fb.fill_round_rect(
                rect_i(border.x + ox, border.y + oy, border.width, border.height),
                radius,
                c,
            );
        }

        // (b) Borders: four filled edge rects, each `border.<side>` thick. With a corner radius we
        // approximate by rounding the outer outline (the inner straight edges still butt the
        // background); the background corners are the visually dominant effect.
        let e = b.dimensions.border;
        let ba = scale_alpha(255, opacity);
        let bc = Color { r: b.style.border_color.0, g: b.style.border_color.1, b: b.style.border_color.2, a: ba };
        let bx = border.x + ox;
        let by = border.y + oy;
        if e.top > 0.0 {
            fb.fill_round_rect(rect_i(bx, by, border.width, e.top), radius.min(e.top.max(1.0)), bc);
        }
        if e.bottom > 0.0 {
            fb.fill_round_rect(rect_i(bx, by + border.height - e.bottom, border.width, e.bottom), radius.min(e.bottom.max(1.0)), bc);
        }
        if e.left > 0.0 {
            fb.fill_rect(rect_i(bx, by, e.left, border.height), bc);
        }
        if e.right > 0.0 {
            fb.fill_rect(rect_i(bx + border.width - e.right, by, e.right, border.height), bc);
        }

        // (c) Text content, at the content rect's baseline. Don't paint into the console area.
        if let layout::BoxContent::Text(s) = &b.content {
            if content.y + oy < clip_bottom {
                let ta = scale_alpha(255, opacity);
                let color = Color { r: b.style.color.0, g: b.style.color.1, b: b.style.color.2, a: ta };
                let x = content.x + ox;
                let baseline = content.y + oy + b.style.font_size * 0.8;
                let end_x = draw_run(
                    fb, font, s, x, baseline, b.style.font_size, color, b.style.bold,
                    b.style.letter_spacing,
                );
                // (c2) text-decoration lines, drawn in the text color across the run width.
                let run_w = (end_x - x).max(0.0);
                if run_w > 0.0 {
                    let thickness = (b.style.font_size / 14.0).clamp(1.0, 2.0).round().max(1.0) as i32;
                    if b.style.underline {
                        // Just below the baseline.
                        let uy = (baseline + 1.0).round() as i32;
                        fb.fill_rect(Rect { x: x.round() as i32, y: uy, w: run_w.round() as i32, h: thickness }, color);
                    }
                    if b.style.line_through {
                        // Roughly the text mid-height (baseline is ~0.8 of em below content top).
                        let my = (baseline - b.style.font_size * 0.3).round() as i32;
                        fb.fill_rect(Rect { x: x.round() as i32, y: my, w: run_w.round() as i32, h: thickness }, color);
                    }
                }
            }
        }

        // (d) Replaced image content: blit the decoded pixels into the content rect, scaled.
        if let layout::BoxContent::Image(node) = &b.content {
            if content.y + oy < clip_bottom {
                let dst = rect_i(content.x + ox, content.y + oy, content.width, content.height);
                match images.get(node) {
                    Some(img) if opacity >= 0.999 => fb.blit_rgba(dst, &img.rgba, img.w, img.h),
                    Some(img) => {
                        // Apply opacity by pre-scaling each source pixel's alpha.
                        let mut scaled = img.rgba.clone();
                        for px in scaled.chunks_exact_mut(4) {
                            px[3] = scale_alpha(px[3], opacity);
                        }
                        fb.blit_rgba(dst, &scaled, img.w, img.h);
                    }
                    None => {
                        // Missing / undecoded image: draw a faint placeholder border.
                        let ph = Color { r: 140, g: 140, b: 150, a: scale_alpha(120, opacity) };
                        if dst.w > 0 && dst.h > 0 {
                            fb.fill_rect(Rect { x: dst.x, y: dst.y, w: dst.w, h: 1 }, ph);
                            fb.fill_rect(Rect { x: dst.x, y: dst.y + dst.h - 1, w: dst.w, h: 1 }, ph);
                            fb.fill_rect(Rect { x: dst.x, y: dst.y, w: 1, h: dst.h }, ph);
                            fb.fill_rect(Rect { x: dst.x + dst.w - 1, y: dst.y, w: 1, h: dst.h }, ph);
                        }
                    }
                }
            }
        }
    }

    for child in &b.children {
        paint_box_opacity(fb, font, child, ox, oy, clip_top, clip_bottom, images, opacity);
    }
}

/// Round an `f32` CSS-pixel rect into a device-pixel [`Rect`].
fn rect_i(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect { x: x.round() as i32, y: y.round() as i32, w: w.round() as i32, h: h.round() as i32 }
}

/// Tags whose subtrees contribute no visible text.
const SKIP_SUBTREE: &[&str] = &["script", "style", "head", "title", "noscript"];

/// Block-ish tags that introduce a line break around their content.
const BLOCK_TAGS: &[&str] = &[
    "p", "div", "h1", "h2", "h3", "h4", "h5", "h6", "li", "br", "section", "article",
    "header", "footer", "ul", "ol", "tr",
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
                process_css_text(&src, base, &mut sheets, &mut console, &mut fetched, &mut seen);
            }
            StyleSource::External(url) => {
                fetch_css(&url, &mut sheets, &mut console, &mut fetched, &mut seen);
            }
        }
    }
    (sheets, console)
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
    sheets.push(css::parse(text));
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
    fn walk(doc: &dom::Document, id: dom::NodeId, base: &str, out: &mut Vec<(dom::NodeId, String)>) {
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
            console.push(format!("[skipped image (limit {MAX_IMAGES} reached): {url}]"));
        }
    }

    // `data:` images decode inline (no I/O); network images are fetched concurrently across a
    // small pool of scoped threads, since they're independent and order doesn't matter.
    let (data_targets, net_targets): (Vec<_>, Vec<_>) =
        targets.into_iter().partition(|(_, url)| url.starts_with("data:"));

    let mut results: Vec<(dom::NodeId, String, Result<DecodedImage, String>)> = Vec::new();
    for (node, url) in data_targets {
        let r = decode_data_url(&url)
            .ok_or_else(|| "malformed data: URL".to_string())
            .and_then(|b| decode_image(&b).ok_or_else(|| "decode failed".to_string()));
        results.push((node, url, r));
    }

    if !net_targets.is_empty() {
        let n_threads = net_targets.len().min(8).max(1);
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
                                let r = net::fetch(&url)
                                    .and_then(|resp| {
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
                let label = if url.starts_with("data:") { "data: image" } else { &url };
                console.push(format!("[failed to load image: {label} — {e}]"));
            }
        }
    }
    images
}

/// Decode raster image bytes into straight-alpha RGBA8. Returns `None` on decode failure or if
/// the decoded image would exceed [`MAX_IMAGE_PIXELS`]. Never panics.
fn decode_image(bytes: &[u8]) -> Option<DecodedImage> {
    let dynimg = image::load_from_memory(bytes).ok()?;
    let w = dynimg.width();
    let h = dynimg.height();
    if (w as u64) * (h as u64) > MAX_IMAGE_PIXELS {
        return None;
    }
    let rgba = dynimg.to_rgba8();
    Some(DecodedImage { rgba: rgba.into_raw(), w, h })
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
        draw_text_spaced(fb, font, text, x + 1.0, baseline_y, px, color, letter_spacing);
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
                                out.push(SpecifierRef { start: k, end, spec });
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
                            out.push(SpecifierRef { start: p, end, spec });
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
                                out.push(SpecifierRef { start: p, end, spec });
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
                notes.push(format!("[skipped module (limit {MAX_MODULES} reached): {u}]"));
            }
            break;
        }
        if frontier.len() > remaining {
            for u in frontier.split_off(remaining) {
                notes.push(format!("[skipped module (limit {MAX_MODULES} reached): {u}]"));
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
            let n = net_urls.len().min(8).max(1);
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
                                        Ok(resp) if resp.body.len() > MAX_MODULE_BYTES => Err(
                                            format!("[skipped large module: {} ({} bytes)]", u, resp.body.len()),
                                        ),
                                        Ok(resp) => Ok(String::from_utf8_lossy(&resp.body).into_owned()),
                                        Err(e) => Err(format!("[failed to load module: {u} — {e}]")),
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
            let base = if url.contains("#inline-module-") { page_url.to_string() } else { url.clone() };
            let specs = extract_specifiers(&body);
            let mut replacements: Vec<(usize, usize, String)> = Vec::new();
            for sp in &specs {
                if is_bare_specifier(&sp.spec) {
                    notes.push(format!("[skipped bare import: {}]", sp.spec));
                    continue;
                }
                let resolved = match url::Url::parse(&base).ok().and_then(|b| b.join(sp.spec.trim()).ok()) {
                    Some(u) => u.to_string(),
                    None => {
                        notes.push(format!("[failed to resolve import: {} (in {url})]", sp.spec));
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
            replacements.sort_by(|a, b| b.0.cmp(&a.0));
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
    let fetcher: Box<dyn Fn(&str) -> Option<String> + Send> = Box::new(|u: &str| {
        net::fetch(u).ok().map(|r| String::from_utf8_lossy(&r.body).into_owned())
    });
    let (doc, results) = js::run_modules(doc, page_url, entries, sources, fetcher);
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
                    slots.push(Slot::Note(format!("[skipped large script: {} bytes]", src.len())));
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
                    Ok(resp) if resp.body.len() > MAX_SCRIPT_BYTES => slots.push(Slot::Note(
                        format!("[skipped large script: {} ({} bytes)]", url, resp.body.len()),
                    )),
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
fn start_session(
    doc: dom::Document,
    base: &str,
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

    if classic.is_empty() && entries.is_empty() {
        return (None, doc, notes);
    }

    let fetcher: Box<dyn Fn(&str) -> Option<String> + Send> = Box::new(|u: &str| {
        net::fetch(u).ok().map(|r| String::from_utf8_lossy(&r.body).into_owned())
    });
    let (session, snapshot, results) =
        js::Session::new(doc, classic, entries, module_sources, base, fetcher);
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
        dom::NodeData::Document => {
            for &child in &doc.get(id).children {
                collect_text(doc, child, out);
            }
        }
        dom::NodeData::Comment(_) => {}
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
        Rect { x: 0, y: top, w: dw as i32, h: (dh as i32 - top).max(0) },
        Color::rgb(14, 15, 20),
    );
    fb.fill_rect(
        Rect { x: 0, y: top, w: dw as i32, h: (2.0 * scale).max(1.0) as i32 },
        Color::rgb(60, 120, 160),
    );

    let left = 12.0 * scale;
    let label_px = 12.0 * scale;
    let line_px = 12.0 * scale;
    let line_h = line_px * 1.35;

    // "console" label just under the divider.
    let mut baseline = top as f32 + label_px + 6.0 * scale;
    draw_text(fb, font, "console", left, baseline, label_px, Color::rgb(120, 200, 255));
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
            fb, font, line, left, &mut line_baseline, line_px, line_h, max_x, max_y, color,
        );
        // Advance past however many wrapped rows this line consumed (at least one).
        baseline = line_baseline.max(baseline + line_h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_ne!(top, scrolled, "scrolling a tall page must change the visible content");

        // Scrolling back to the top restores the original view (clamped at 0).
        e.scroll_by(-100000.0);
        let back = center_column(e.render()).clone();
        assert_eq!(top, back, "scrolling back to the top restores the original render");

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
        assert!(e.focus_first_text_field(), "page must have an editable text field");
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

        assert!(e.dispatch_click(cx, cy), "clicking the checkbox warrants a re-render");
        let c2 = e.node_by_attr_id("c").expect("checkbox node");
        assert!(e.node_attr(c2, "checked").is_some(), "checkbox should be checked after click");
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
        assert!(e.dispatch_move(cx, cy), "moving over a new node should change hover");
        assert_eq!(e.visible_attr_body("data-hover").as_deref(), Some("yes"));

        // Moving again to the same node is a cheap no-op (returns false).
        let _ = e.render();
        let m2 = e.node_by_attr_id("m").expect("menu node");
        let (cx2, cy2) = e.node_center_device(m2).expect("menu laid out");
        assert!(!e.dispatch_move(cx2, cy2), "hovering the same node again is a no-op");

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
        assert!(e.focused_node_for_test().is_none(), "focus cleared after clicking outside");

        let _ = std::fs::remove_file(&path);
    }

    fn base64_encode(data: &[u8]) -> String {
        const A: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            out.push(A[(n >> 18 & 63) as usize] as char);
            out.push(A[(n >> 12 & 63) as usize] as char);
            out.push(if chunk.len() > 1 { A[(n >> 6 & 63) as usize] as char } else { '=' });
            out.push(if chunk.len() > 2 { A[(n & 63) as usize] as char } else { '=' });
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
        let doc = html::parse(r#"<html><body><script>console.log("hi", 6*7)</script></body></html>"#);
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
        assert_eq!(resolve_url("https://a.com/x/y.html", "data:text/css,a{}"), None);
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
        assert_eq!(base_url(&doc, "https://orig.com/page.html"), "https://cdn.example/assets/");
        // A relative <base href> resolves against the response URL.
        let doc2 = html::parse(r#"<html><head><base href="/sub/"></head></html>"#);
        assert_eq!(base_url(&doc2, "https://orig.com/a/b.html"), "https://orig.com/sub/");
        // No <base>: falls back to the response URL.
        let doc3 = html::parse("<html><head></head></html>");
        assert_eq!(base_url(&doc3, "https://orig.com/a/b.html"), "https://orig.com/a/b.html");
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
            sheets[1].rules.iter().any(|r| r.selectors.iter().any(|s| s.contains('p'))),
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
        std::fs::write(&main_path, "@import \"tokens.css\";\n.main { color: #222222 }").unwrap();

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
            sheets[0].rules.iter().any(|r| r.selectors.iter().any(|s| s == ".token")),
            "imported tokens.css should come first: {sheets:?}"
        );
        assert!(
            sheets[1].rules.iter().any(|r| r.selectors.iter().any(|s| s == ".main")),
            "importer main.css should come second: {sheets:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn script_errors_are_captured_as_warnings() {
        let doc = html::parse(r#"<html><body><script>throw new Error("boom")</script></body></html>"#);
        let (_doc, console) = run_scripts(doc, "https://example.com/");
        assert!(console.iter().any(|l| l.starts_with('⚠') && l.contains("boom")), "got {console:?}");
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
        fn find<'a>(doc: &'a dom::Document, tag: &str) -> dom::NodeId {
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
        paint_box(&mut fb, &NoFont, &root, 16.0, 28.0, 28.0, 300.0, &images);

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
            let root = layout::layout_document(&doc, &computed, 100.0, 200.0, &M, &HashMap::new(), None);
            let mut fb = Framebuffer::new(100, 100);
            paint_gradient(&mut fb);
            let imgs: HashMap<dom::NodeId, DecodedImage> = HashMap::new();
            paint_box(&mut fb, &NoFont, &root, 0.0, 0.0, 0.0, 200.0, &imgs);
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
        assert!(half[0] < opaque[0], "half {:?} should be darker than opaque {:?}", half, opaque);
        assert!(half[0] > 80, "half white over dark should still be fairly light, r={}", half[0]);
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
        let html = format!(
            r#"<html><body><img src="{img_url}"></body></html>"#
        );
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
        let (sheets, _console) = collect_stylesheets(&doc, &format!("file://{}", html_path.display()));
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
        assert_eq!(entries.len(), 2, "classic script must be excluded: {entries:?}");
        assert_eq!(entries[0], ModuleEntry::External("https://x.com/app.js".to_string()));
        assert!(matches!(&entries[1], ModuleEntry::Inline(s) if s.contains("./side.js")));
        // Classic scripts are NOT collected as modules.
        let classic = collect_script_sources(&doc, "https://x.com/page/");
        assert!(classic.iter().any(|s| matches!(s, ScriptSource::External(u) if u.ends_with("classic.js"))));
        // ...and the module scripts are skipped by the classic collector.
        assert!(!classic.iter().any(|s| matches!(s, ScriptSource::External(u) if u.ends_with("app.js"))));
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
            notes.iter().any(|n| n.contains("[skipped bare import: vue]")),
            "expected bare-import note, got {notes:?}"
        );
    }
}
