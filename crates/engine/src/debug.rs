use crate::*;

impl Engine {
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
    pub(crate) fn focus_first_text_field(&mut self) -> bool {
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
    pub(crate) fn body_attr(&self, name: &str) -> Option<String> {
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
    pub(crate) fn node_attr(&self, id: dom::NodeId, name: &str) -> Option<String> {
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
    pub(crate) fn focused_node_for_test(&self) -> Option<dom::NodeId> {
        self.focused_node
    }

    /// Test-only: number of decoded `<img>` images for the current page.
    #[cfg(test)]
    pub(crate) fn decoded_image_count(&self) -> usize {
        match &self.state {
            LoadState::Loaded { images, .. } => images.len(),
            _ => 0,
        }
    }

    /// Test-only: the (w, h) of the first decoded image, if any.
    #[cfg(test)]
    pub(crate) fn first_decoded_image_size(&self) -> Option<(u32, u32)> {
        match &self.state {
            LoadState::Loaded { images, .. } => images.values().next().map(|i| (i.w, i.h)),
            _ => None,
        }
    }

    /// Test-only: device-pixel center of the cached layout box for `id` (inverse of the
    /// layout→device mapping in `render`: left=0, header_h=0, so device = layout - scroll_y).
    #[cfg(test)]
    pub(crate) fn node_center_device(&self, id: dom::NodeId) -> Option<(f32, f32)> {
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
    pub(crate) fn node_device_rect(&self, id: dom::NodeId) -> Option<layout::Rect> {
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
    pub(crate) fn node_by_attr_id(&self, attr_id: &str) -> Option<dom::NodeId> {
        match &self.state {
            LoadState::Loaded { doc: Some(d), .. } => find_by_attr_id(d, d.root(), attr_id),
            _ => None,
        }
    }

    /// Test-only: the value of attribute `name` on the live document's `<body>`.
    #[cfg(test)]
    pub(crate) fn visible_attr_body(&self, name: &str) -> Option<String> {
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
