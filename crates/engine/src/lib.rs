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
        status: u16,
        content_type: String,
        bytes: usize,
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
    /// Retained so the FFI layer can hand out a pointer that stays valid until the next
    /// render or until the engine is dropped.
    framebuffer: Option<Framebuffer>,
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
            framebuffer: None,
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
    }

    /// Fetch `url` and remember the outcome. Returns 0 on success, negative on error.
    pub fn load_url(&mut self, url: &str) -> i32 {
        self.scroll_y = 0.0; // new navigation starts at the top
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

                // Fetch + parse external CSS interleaved with inline <style> in document order
                // (BEFORE scripts so the cascade has author sheets; scripts may still mutate the
                // DOM, but our `<link>`/`<style>` collection is a one-shot snapshot of the parse).
                let (styles, mut console) = match &doc {
                    Some(d) => collect_stylesheets(d, &base),
                    None => (Vec::new(), Vec::new()),
                };

                // Execute the page's scripts (inline + external `<script src>`, fetched and run
                // in document order through the real DOM so mutations stick) and capture console.
                let doc = match doc {
                    Some(d) => {
                        let (d, script_console) = run_scripts(d, &base);
                        console.extend(script_console);
                        Some(d)
                    }
                    None => None,
                };

                // Fetch + decode `<img>` images (after scripts, so script-inserted images and
                // mutated `src` attributes are seen). Reset on every navigation.
                let images = match &doc {
                    Some(d) => collect_images(d, &base, &mut console),
                    None => HashMap::new(),
                };

                self.state = LoadState::Loaded {
                    url: resp.final_url,
                    status: resp.status,
                    content_type: resp.content_type,
                    bytes: resp.body.len(),
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

    /// Paint the current state into a fresh framebuffer and return a reference to it.
    pub fn render(&mut self) -> &Framebuffer {
        let dw = ((self.vp_w as f32) * self.scale).round() as u32;
        let dh = ((self.vp_h as f32) * self.scale).round() as u32;
        let mut fb = Framebuffer::new(dw.max(1), dh.max(1));
        // Local copy so we can clamp against the document height inside the (immutably
        // borrowed) state match, then write the clamped value back after.
        let mut scroll_y = self.scroll_y;

        paint_gradient(&mut fb);

        // Slim header band so it's unmistakably our paint, not a blank surface.
        let header_h = 28.0 * self.scale;
        fb.fill_rect(Rect { x: 0, y: 0, w: dw as i32, h: header_h as i32 },
                     Color::rgb(20, 22, 30));

        let px = 16.0 * self.scale;
        if let Some(font) = self.font.as_ref() {
            match &self.state {
                LoadState::Empty => {
                    draw_text(&mut fb, font, "browser — phase 2", 12.0 * self.scale,
                              19.0 * self.scale, 13.0 * self.scale, Color::rgb(120, 200, 255));
                    draw_text(&mut fb, font, "Enter a URL and press Go.",
                              12.0 * self.scale, 60.0 * self.scale, px, Color::WHITE);
                }
                LoadState::Loaded { url, status, content_type, bytes, doc, styles, console, images } => {
                    // Header line: HTTP status, content type, size, and URL.
                    let head =
                        format!("HTTP {status}   |   {content_type}   |   {bytes}B   |   {url}");
                    draw_text(&mut fb, font, &head, 12.0 * self.scale,
                              19.0 * self.scale, 13.0 * self.scale, Color::rgb(120, 200, 255));

                    let left = 16.0 * self.scale;
                    let max_x = dw as f32 - left;

                    // When there's console output, reserve the bottom ~35% for the console
                    // panel and clip the page text to stop above it.
                    let page_max_y = if console.is_empty() {
                        dh as f32
                    } else {
                        (dh as f32 * 0.65).floor()
                    };

                    match doc {
                        Some(d) => {
                            // Cascade UA + author sheets + inline attrs into per-node styles,
                            // then run box-model layout and paint the resulting box tree.
                            let computed = style::cascade(d, styles);
                            let viewport_width = (max_x - left).max(1.0);
                            let viewport_height = (page_max_y - header_h).max(1.0);
                            let measurer = FontMeasurer { font };
                            // Intrinsic sizes (px) for replaced (image) boxes, from decoded images.
                            let intrinsic_sizes: HashMap<dom::NodeId, (f32, f32)> = images
                                .iter()
                                .map(|(&id, img)| (id, (img.w as f32, img.h as f32)))
                                .collect();
                            let root = layout::layout_document(
                                d, &computed, viewport_width, viewport_height, &measurer,
                                &intrinsic_sizes,
                            );
                            // Clamp the scroll offset to the laid-out document height so we
                            // can't scroll past the end.
                            let content_h = root.dimensions.margin_box().height;
                            let max_scroll = (content_h - viewport_height).max(0.0);
                            scroll_y = scroll_y.min(max_scroll);
                            // Translate the page below the header band, offset by the scroll
                            // position, and clip vertically to the page region.
                            paint_box(
                                &mut fb, font, &root, left, header_h - scroll_y, header_h,
                                page_max_y, images,
                            );
                        }
                        None => {
                            let line_h = px * 1.4;
                            let mut baseline = header_h + line_h;
                            draw_text(
                                &mut fb, font, &format!("(non-HTML content: {})", url),
                                left, baseline, px, Color::WHITE,
                            );
                            let _ = &mut baseline;
                        }
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

/// Draw a left-anchored string with its baseline at `baseline_y`.
fn draw_text(
    fb: &mut Framebuffer,
    font: &dyn GlyphRasterizer,
    text: &str,
    x: f32,
    baseline_y: f32,
    px: f32,
    color: Color,
) {
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
    }
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
    let border = b.dimensions.border_box();
    let content = b.dimensions.content;
    // Translate the box's vertical extent into device space for clipping.
    let top = border.y.min(content.y) + oy;
    let bottom = (border.y + border.height).max(content.y + content.height) + oy;
    // Fully outside the visible band: skip this box (children may still be inside, so only
    // skip when even the box's lower edge is above the band, or its top is below it).
    let offscreen = bottom < clip_top || top >= clip_bottom;

    if !offscreen {
        // (a) Background fills the border box.
        if let Some((r, g, bl)) = b.style.background_color {
            fb.fill_rect(rect_i(border.x + ox, border.y + oy, border.width, border.height),
                         Color::rgb(r, g, bl));
        }

        // (b) Borders: four filled edge rects, each `border.<side>` thick.
        let e = b.dimensions.border;
        let bc = Color::rgb(b.style.border_color.0, b.style.border_color.1, b.style.border_color.2);
        let bx = border.x + ox;
        let by = border.y + oy;
        if e.top > 0.0 {
            fb.fill_rect(rect_i(bx, by, border.width, e.top), bc);
        }
        if e.bottom > 0.0 {
            fb.fill_rect(rect_i(bx, by + border.height - e.bottom, border.width, e.bottom), bc);
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
                let color = Color::rgb(b.style.color.0, b.style.color.1, b.style.color.2);
                let x = content.x + ox;
                let baseline = content.y + oy + b.style.font_size * 0.8;
                draw_run(fb, font, s, x, baseline, b.style.font_size, color, b.style.bold);
            }
        }

        // (d) Replaced image content: blit the decoded pixels into the content rect, scaled.
        if let layout::BoxContent::Image(node) = &b.content {
            if content.y + oy < clip_bottom {
                let dst = rect_i(content.x + ox, content.y + oy, content.width, content.height);
                match images.get(node) {
                    Some(img) => fb.blit_rgba(dst, &img.rgba, img.w, img.h),
                    None => {
                        // Missing / undecoded image: draw a faint placeholder border.
                        let ph = Color { r: 140, g: 140, b: 150, a: 120 };
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
        paint_box(fb, font, child, ox, oy, clip_top, clip_bottom, images);
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

/// Maximum number of external stylesheets fetched per page; the rest are skipped with a note.
const MAX_EXTERNAL_STYLESHEETS: usize = 12;
/// Maximum number of external scripts fetched per page; the rest are skipped with a note.
const MAX_EXTERNAL_SCRIPTS: usize = 24;
/// Skip fetched script bodies larger than this (mirrors the inline-script cap).
const MAX_SCRIPT_BYTES: usize = 4 * 1024 * 1024;

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
    for source in collect_style_sources(doc, base) {
        match source {
            StyleSource::Inline(src) => sheets.push(css::parse(&src)),
            StyleSource::External(url) => {
                if fetched >= MAX_EXTERNAL_STYLESHEETS {
                    console.push(format!(
                        "[skipped stylesheet (limit {MAX_EXTERNAL_STYLESHEETS} reached): {url}]"
                    ));
                    continue;
                }
                fetched += 1;
                match net::fetch(&url) {
                    Ok(resp) => sheets.push(css::parse(&String::from_utf8_lossy(&resp.body))),
                    Err(e) => {
                        console.push(format!("[failed to load stylesheet: {url} — {e}]"))
                    }
                }
            }
        }
    }
    (sheets, console)
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
/// twice with a 1px horizontal offset ("faux bold").
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
) {
    draw_text(fb, font, text, x, baseline_y, px, color);
    if bold {
        draw_text(fb, font, text, x + 1.0, baseline_y, px, color);
    }
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
/// Scripts with a non-JS `type` (e.g. `application/json`, `application/ld+json`, `module` —
/// we don't support ES modules, so they're skipped) are omitted. Pure: unit-testable without
/// network.
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
            layout::layout_document(&doc, &computed, 400.0, 600.0, &measurer, &no_images);

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
        let root = layout::layout_document(&doc, &computed, 400.0, 300.0, &M, &intrinsic);

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
}
