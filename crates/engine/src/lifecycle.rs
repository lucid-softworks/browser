use crate::*;

impl Engine {
    pub fn new() -> Self {
        Engine {
            vp_w: 800,
            vp_h: 600,
            scale: 1.0,
            state: LoadState::Empty,
            font: SystemFont::load(),
            font_faces: HashMap::new(),
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
            favicon: None,
            bg_sources: HashMap::new(),
            bg_bitmaps: HashMap::new(),
            canvas_bg: None,
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
    /// The URL currently committed in this engine — the *resolved* final URL after fixup, HSTS
    /// upgrade, redirects, and any http fallback — or `None` if nothing has loaded. Shells read this
    /// after a load so the address bar shows the real address (e.g. the http page a defaulted-https
    /// navigation fell back to, or the https page an HSTS pin forced).
    pub fn current_url(&self) -> Option<&str> {
        match &self.state {
            LoadState::Loaded { url, .. } | LoadState::Failed { url, .. } => Some(url.as_str()),
            LoadState::Empty => None,
        }
    }

    /// The current page's decoded favicon as straight-alpha RGBA8 `(pixels, width, height)`, or
    /// `None` if the page has no (loadable) icon. The shell renders it in the tab + address bar.
    pub fn favicon(&self) -> Option<(&[u8], u32, u32)> {
        self.favicon.as_ref().map(|f| (f.rgba.as_slice(), f.w, f.h))
    }

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
        self.favicon = None; // and no site icon until this page's loads
        net::clear_network_log(); // devtools Network tab tracks this navigation's requests

        // URL fixup (shared with every other shell via `net::fixup_url`): a schemeless address-bar
        // entry becomes `https://…`; authority-less schemes (`about:blank`, `data:…`) pass through.
        let fixup = net::fixup_url(url);
        let streaming = self.frame_cb.is_some();

        // Stream the body: re-parse on each chunk and paint throttled partial frames. We also
        // accumulate the raw bytes so the non-HTML branch / content sniffing below can inspect the
        // full body without depending on the streaming parser's internal buffer.
        //
        // A schemeless address we defaulted to https that can't *connect* falls back to http once
        // (some sites are http-only), unless HSTS pins the host to https. A real HTTP error status
        // is NOT a fallback trigger. Each attempt streams into a fresh parser.
        let mut target = fixup.url;
        let (parser, result) = loop {
            let mut parser = html::StreamParser::new();
            let mut last_paint: Option<Instant> = None;
            let result = net::fetch_streaming(&target, &mut |chunk| {
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
                self.install_partial(snapshot, &target);
                self.emit_partial_frame();
            });
            if let Err(e) = &result {
                if fixup.https_defaulted
                    && target.starts_with("https://")
                    && net::is_connection_error(e)
                    && !net::hsts_pinned_url(&target)
                {
                    target = format!("http://{}", &target["https://".len()..]);
                    continue;
                }
            }
            break (parser, result);
        };

        match result {
            Ok(meta) => {
                // Cross-origin isolation (COOP+COEP on this navigation) drives self.crossOriginIsolated
                // in the page's JS (and any worker it spawns). Set before the session/scripts start.
                js::set_cross_origin_isolated(meta.cross_origin_isolated);
                // HTML when the server says so, OR when the type is unknown/generic and the body
                // sniffs as HTML (mirrors the old `content_type.contains("html")` gate, extended
                // with a structural sniff for type-less responses).
                let ct = meta.content_type.to_ascii_lowercase();
                // Navigating directly to an image: wrap it in a tiny generated document so it renders
                // as a picture (like real browsers) instead of showing raw bytes.
                let is_image = ct.starts_with("image/")
                    || (!ct.contains("html") && url_has_image_extension(&meta.final_url));
                let final_doc = if is_image {
                    html::parse(&image_viewer_html(&meta.final_url))
                } else {
                    parser.finish()
                };
                let looks_html = is_image
                    || ct.contains("html")
                    || (ct.is_empty() || ct == "application/octet-stream")
                        && document_looks_like_html(&final_doc);

                // Build the FINAL state exactly as the non-streaming path did.
                let base = if looks_html {
                    base_url(&final_doc, &meta.final_url)
                } else {
                    meta.final_url.clone()
                };

                let mut console: Vec<String> = Vec::new();
                // Resolve the favicon URL from the parsed document (pre-script `<link rel=icon>` /
                // origin fallback) before `final_doc` is moved into the JS session; it's fetched +
                // decoded below once the page has settled.
                let favicon_url = if looks_html {
                    resolve_favicon_url(&final_doc, &base)
                } else {
                    None
                };
                let doc = if looks_html {
                    // Pre-layout the parsed document (inline-CSS only, no network) so the JS session
                    // can seed `layout_rects` BEFORE its scripts run. Synchronous layout-dependent
                    // reads during load (getBoundingClientRect / elementFromPoint / caret*FromPoint)
                    // then see real geometry instead of 0/null. Best-effort: external CSS/images
                    // aren't loaded yet, and the engine re-lays-out + re-pushes authoritatively after.
                    // Load @font-face web fonts before scripts so the seed layout measures text in
                    // the right font (the inline cascade below names the family; the face is loaded
                    // here from the page's stylesheets, including external ones like WPT's ahem.css).
                    self.load_web_fonts_early(&final_doc, &base);
                    let initial_rects = {
                        let inline_styles = collect_inline_stylesheets(&final_doc, &base);
                        let no_images: HashMap<dom::NodeId, DecodedImage> = HashMap::new();
                        compute_initial_rects(
                            &final_doc,
                            &inline_styles,
                            &no_images,
                            self.font.as_ref(),
                            &self.font_faces,
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

                // Pick up any @font-face declared in runtime-injected stylesheets (most are already
                // loaded by `load_web_fonts_early` before scripts; this catches script-added ones).
                self.load_web_fonts(&styles, &base);
                // Fetch + decode the site favicon (best-effort; the page is already painted via the
                // streaming/final frame, so this only gates the shell's icon update).
                self.favicon = favicon_url.and_then(|u| fetch_favicon(&u, self.font.as_ref()));
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
                    // Compose background-image bitmaps now so the first paint shows them (render's
                    // own ensure_layout sees a valid cache and won't recompose them).
                    self.update_bg_image_bitmaps();
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
                    url: target,
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
    pub(crate) fn install_partial(&mut self, doc: dom::Document, url: &str) {
        let base = base_url(&doc, url);
        let styles = collect_inline_stylesheets(&doc, &base);
        self.load_web_fonts(&styles, &base);
        self.state = LoadState::Loaded {
            url: url.to_string(),
            doc: Some(doc),
            styles,
            console: Vec::new(),
            images: HashMap::new(),
        };
        self.layout_cache = None;
    }

    /// Load `@font-face` web fonts BEFORE the page's scripts run, so the script-visible seed layout
    /// (and `getBoundingClientRect`/`offsetWidth` reads in `document.fonts.ready.then(...)`) reflect
    /// web-font metrics. Gated: only fetches external CSS when the page actually has external
    /// stylesheets or an inline `@font-face`, so font-free inline-only pages pay nothing. (When it
    /// does fetch, those sheets are fetched again for the post-script authoritative layout — a small
    /// cost paid only by pages that ship CSS, in exchange for correct first-layout font metrics.)
    pub(crate) fn load_web_fonts_early(&mut self, doc: &dom::Document, base: &str) {
        let sources = collect_style_sources(doc, base);
        let has_external = sources
            .iter()
            .any(|s| matches!(s, StyleSource::External(_)));
        let inline_font_face = sources
            .iter()
            .any(|s| matches!(s, StyleSource::Inline(t) if t.contains("@font-face")));
        if !has_external && !inline_font_face {
            return;
        }
        let (styles, _console) = collect_stylesheets(doc, base);
        self.load_web_fonts(&styles, base);
    }

    /// Fetch and register every `@font-face` declared in `styles`, so text whose `font-family` names
    /// a declared face is measured/painted with that font. Relative `src` URLs resolve against the
    /// stylesheet's own base (else `doc_base`). Already-loaded families and unfetchable/undecodable
    /// sources (e.g. `woff2`, which fontdue can't parse) are skipped. Best-effort: any failure just
    /// leaves the system font in use for that family. Called BEFORE the page's scripts run so the
    /// first (script-visible) layout already reflects web-font metrics.
    pub(crate) fn load_web_fonts(&mut self, styles: &[css::Stylesheet], doc_base: &str) {
        for ff in styles.iter().flat_map(|s| s.font_face_rules.iter()) {
            let key = ff.family.to_ascii_lowercase();
            if self.font_faces.contains_key(&key) {
                continue;
            }
            let sheet_base = ff.base_url.as_deref().unwrap_or(doc_base);
            for src in &ff.src {
                let Some(abs) = resolve_url(sheet_base, src) else {
                    continue;
                };
                if let Ok(resp) = net::fetch(&abs) {
                    if let Some(face) = SystemFont::from_bytes(resp.body) {
                        self.font_faces.insert(key.clone(), face);
                        break;
                    }
                }
            }
        }
    }

    /// Paint the current state into `self.framebuffer` and hand a borrowed [`FrameView`] to the
    /// installed frame callback. The borrow choreography: `render(&mut self)` finishes (releasing the
    /// `&mut self` borrow) and stores the framebuffer on `self`; we then read the (now immutable)
    /// buffer fields and the `Copy` callback tuple out of `self` and invoke it — so `self` is never
    /// borrowed mutably while we read its framebuffer for the callback.
    pub(crate) fn emit_partial_frame(&mut self) {
        self.render();
        self.dispatch_frame();
    }

    /// Like [`emit_partial_frame`] but named for the terminal paint; identical mechanics.
    pub(crate) fn emit_final_frame(&mut self) {
        self.render();
        self.dispatch_frame();
    }

    /// Read the last-painted framebuffer and forward it to the frame callback (if any). Pulled out
    /// so the `&mut self` render borrow is fully released before we read the buffer for the callback.
    pub(crate) fn dispatch_frame(&mut self) {
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
    pub(crate) fn ensure_layout(&mut self, dw: u32, dh: u32, header_h: f32) -> bool {
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
            // Lay out against the LOGICAL (CSS-px) viewport, not the device framebuffer: a real
            // browser lays out in CSS px and rasterizes at the backing scale. `dw`/`dh` are device
            // px (= logical × scale), so divide back out here. The resulting CSS-px box tree is
            // scaled to device px by `scale_layout_tree` below, which is the space the rest of the
            // engine (painter, hit-testing, `getBoundingClientRect` ÷ scale, scrollbar) works in.
            // This keeps the cascade's viewport metrics (media queries, `vw`/`vh`) — fed the logical
            // size at line ~315 — consistent with the layout viewport.
            let vw = (dw as f32 / self.scale).max(1.0);
            let vh = ((page_max_y - header_h) / self.scale).max(1.0);
            let measurer = FontMeasurer {
                font,
                faces: &self.font_faces,
            };
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
            let mut root = layout::layout_document(
                d,
                &computed,
                vw,
                vh,
                &measurer,
                &intrinsic_sizes,
                self.focused_node,
            );
            // CSS px → device px: bake the backing scale into the box tree so every downstream
            // consumer keeps working in device px (the painter blits 1:1, hit-testing matches the
            // device-px cursor, `getBoundingClientRect` divides back to CSS px).
            scale_layout_tree(&mut root, self.scale);
            let content_h = root.dimensions.margin_box().height;
            Some((root, content_h, root_scheme_dark, computed))
        } else {
            None
        };
        self.layout_cache =
            computed.map(|(root, content_h, root_scheme_dark, styles)| LayoutCache {
                dw,
                dh,
                root,
                content_h,
                root_scheme_dark,
                styles,
            });
        true
    }

    /// Rebuild [`Self::canvas_bitmaps`] from the JS 2D-context display lists. Pulls every canvas's
    /// `{id,width,height,commands}` via the Session, parses the JSON, and rasterizes each command
    /// stream into an RGBA bitmap. Guarded: returns immediately if there's no script Session or the
    /// loaded DOM contains no `<canvas>` (the common case), so non-canvas pages pay nothing.
    pub(crate) fn update_canvas_bitmaps(&mut self) {
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
    pub(crate) fn update_svg_bitmaps(&mut self) {
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
        let svg_styles = self.layout_cache.as_ref().map(|c| &c.styles);
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
            let bmp = svg::rasterize_svg(doc, id, w as u32, h as u32, font, svg_styles);
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
    pub(crate) fn update_mask_bitmaps(&mut self) {
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

    /// Build a per-box `background-image` bitmap for every box with `extras.background_image`: fetch
    /// + decode each distinct source once (cached in `self.bg_sources` keyed by url), then compose it
    /// into the box's border-box device size honoring `background-size`/`-repeat`/`-position`. The
    /// painter blits the result (source-over) atop the box's background color. Background-free pages
    /// clear the caches and pay nothing. Call AFTER `ensure_layout`.
    pub(crate) fn update_bg_image_bitmaps(&mut self) {
        let mut targets: Vec<(dom::NodeId, layout::Rect, style::BgImage)> = Vec::new();
        if let Some(cache) = &self.layout_cache {
            collect_bg_targets(&cache.root, &mut targets);
        }
        if targets.is_empty() {
            if !self.bg_bitmaps.is_empty() {
                self.bg_bitmaps.clear();
            }
            if !self.bg_sources.is_empty() {
                self.bg_sources.clear();
            }
            self.canvas_bg = None;
            return;
        }

        // url()s are resolved against their stylesheet base during the cascade, so they're normally
        // absolute; the page URL is a FALLBACK for inline styles / base-less sheets.
        let base = match &self.state {
            LoadState::Loaded { url, .. } => url.clone(),
            _ => String::new(),
        };
        for (_, _, bg) in &targets {
            if self.bg_sources.contains_key(&bg.url) {
                continue;
            }
            if let Some(src) = load_bg_source(&bg.url, &base) {
                self.bg_sources.insert(bg.url.clone(), src);
            }
        }
        let live: std::collections::HashSet<&String> =
            targets.iter().map(|(_, _, b)| &b.url).collect();
        self.bg_sources.retain(|k, _| live.contains(k));

        // CSS background propagation: the root (`<html>`) background image — or the `<body>`'s when
        // html has none — is painted on the canvas at the viewport size, not on its own box. (Without
        // this, a body image tiles to the body box's height, which varies with content.)
        self.canvas_bg = None;
        let prop = self.layout_cache.as_ref().and_then(|cache| {
            // Walk the first-child chain (root → html → body) for the first element with a background
            // image — that's the one whose background propagates to the canvas.
            let mut node = &cache.root;
            for _ in 0..3 {
                if let Some(nid) = node.node {
                    if targets.iter().any(|(n, _, _)| *n == nid) {
                        return Some((nid, cache.content_h));
                    }
                }
                match node.children.first() {
                    Some(c) => node = c,
                    None => break,
                }
            }
            None
        });
        if let Some((pid, content_h)) = prop {
            if let Some(pos) = targets.iter().position(|(n, _, _)| *n == pid) {
                let (_, _, bg) = targets.remove(pos);
                let vw = (self.vp_w as f32 * self.scale).round().clamp(1.0, 8192.0) as u32;
                let vh = ((self.vp_h as f32 * self.scale).max(content_h))
                    .round()
                    .clamp(1.0, 8192.0) as u32;
                if let Some(src) = self.bg_sources.get(&bg.url) {
                    self.canvas_bg = Some(compose_background(src, vw, vh, &bg));
                }
            }
        }

        let mut next: HashMap<dom::NodeId, DecodedImage> = HashMap::new();
        for (id, rect, bg) in targets {
            let src = match self.bg_sources.get(&bg.url) {
                Some(s) => s,
                None => continue, // fetch/decode failed
            };
            let w = rect.width.round().clamp(1.0, 8192.0) as u32;
            let h = rect.height.round().clamp(1.0, 8192.0) as u32;
            next.insert(id, compose_background(src, w, h, &bg));
        }
        self.bg_bitmaps = next;
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
    pub(crate) fn push_layout_rects(&self) {
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
        let layout_changed = self.ensure_layout(dw, dh, header_h);
        if layout_changed {
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
        // Compose per-box `background-image` bitmaps. Only when layout was (re)built — these can be
        // large (full-page backgrounds), so we don't recompose on every scroll repaint.
        if layout_changed {
            self.update_bg_image_bitmaps();
        }

        let mut fb = Framebuffer::new(dw, dh);
        let mut scroll_y = self.scroll_y;

        // A loaded HTML page paints on a real document canvas (white by default, or the html/body
        // background). The splash / non-HTML / error states (no layout tree) keep the chrome gradient.
        match (&self.state, &self.layout_cache) {
            (LoadState::Loaded { .. }, Some(cache)) => {
                fb.clear(page_background(&cache.root, cache.root_scheme_dark));
                // Then the propagated root/body background image (CSS background propagation), at the
                // canvas origin, scrolled with the page.
                if let Some(bg) = &self.canvas_bg {
                    let dst = Rect {
                        x: 0,
                        y: -(self.scroll_y.round() as i32),
                        w: bg.w as i32,
                        h: bg.h as i32,
                    };
                    fb.blit_rgba(dst, &bg.rgba, bg.w, bg.h);
                }
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
                        // Forced-colors backplate pre-pass: paint every line's Canvas backplate
                        // BEFORE any glyphs, so adjacent inline fragments on one line don't overwrite
                        // each other's text. Spans the full line box (the WPT refs use block bgs).
                        paint_backplates(
                            &mut fb,
                            &cache.root,
                            left,
                            header_h - scroll_y,
                            header_h,
                            page_max_y,
                            (0.0, dw as f32),
                        );
                        paint_box(
                            &mut fb,
                            Fonts {
                                system: font,
                                faces: &self.font_faces,
                            },
                            &cache.root,
                            left,
                            header_h - scroll_y,
                            header_h,
                            page_max_y,
                            images,
                            &self.canvas_bitmaps,
                            &self.svg_bitmaps,
                            &self.mask_bitmaps,
                            &self.bg_bitmaps,
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
}
