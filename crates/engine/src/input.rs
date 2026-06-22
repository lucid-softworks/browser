use crate::*;

impl Engine {
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
    pub(crate) fn selection_ranges(&self, runs: &[TextRun]) -> Vec<Option<(usize, usize)>> {
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
    pub(crate) fn apply_pending_scroll(&mut self) -> bool {
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
}
