use crate::*;

/// A [`layout::TextMeasurer`] backed by our [`SystemFont`], so layout can size text without
/// knowing about font rasterization. Widths mirror what the painter actually draws.
pub(crate) struct FontMeasurer<'a> {
    pub(crate) font: &'a SystemFont,
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
pub(crate) fn deepest_node_at(b: &layout::LayoutBox, x: f32, y: f32) -> Option<dom::NodeId> {
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
pub(crate) fn collect_options(doc: &dom::Document, select_id: dom::NodeId) -> Vec<dom::NodeId> {
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
pub(crate) fn option_text(doc: &dom::Document, opt: dom::NodeId) -> String {
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
pub(crate) fn selected_option_index(
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
pub(crate) enum ObsKind {
    Io,
    Ro,
}

/// One observed IntersectionObserver/ResizeObserver target (parsed from `__observedTargets()`).
pub(crate) struct ObservedTarget {
    pub(crate) kind: ObsKind,
    pub(crate) observer_id: u64,
    pub(crate) node_id: usize,
}

/// Parse the `[{kind,observerId,nodeId}, ...]` JSON produced by `__observedTargets()`. Hand-rolled
/// (no serde dep): scans for the three fields per object. Returns `None` only on a malformed list.
pub(crate) fn parse_observed_targets(json: &str) -> Option<Vec<ObservedTarget>> {
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
pub(crate) fn json_number_field(obj: &str, field: &str) -> Option<f64> {
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
pub(crate) struct TextRun {
    pub(crate) rect: layout::Rect,
    pub(crate) text: String,
    pub(crate) font_size: f32,
    pub(crate) letter_spacing: f32,
}

/// Walk the layout tree depth-first, collecting every `Text` run in reading (paint) order. Each
/// run carries its absolute content rect (document space). This is the ordered list selection
/// resolution and highlight painting both index into.
pub(crate) fn collect_text_runs(root: &layout::LayoutBox) -> Vec<TextRun> {
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
pub(crate) fn char_index_in_run(run: &TextRun, font: &SystemFont, x: f32) -> usize {
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
pub(crate) fn resolve_text_position(
    runs: &[TextRun],
    font: &SystemFont,
    p: Point,
) -> (usize, usize) {
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
pub(crate) const MAX_DOM_DEPTH: usize = 512;

/// Serialize a single DOM node (and its subtree) into `out` as the JSON object documented on
/// [`Engine::dom_tree_json`]. Returns `false` (and writes nothing) when the node is an empty /
/// all-whitespace text node (or a non-rendered node kind), so callers can skip it.
pub(crate) fn serialize_dom_node(
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

/// Bake the backing scale (device pixel ratio) into a laid-out box tree, converting it from the
/// CSS-px space layout runs in to the device-px space the rest of the engine paints and hit-tests
/// in. Multiply every absolute length — box geometry, the box-model edges, font/line/letter
/// metrics, positioned insets/margins, and the absolute paint extras (corner radius, shadow
/// offsets/blur/spread, `transform` translation) — by `s`. Values already expressed as fractions of
/// the box (gradient stops, `transform-origin`, the transform's linear part) are scale-invariant and
/// left untouched. A `scale` of 1.0 (non-Retina) is a no-op.
pub(crate) fn scale_layout_tree(b: &mut layout::LayoutBox, s: f32) {
    if s == 1.0 {
        return;
    }
    fn scale_edges(e: &mut layout::Edges, s: f32) {
        e.top *= s;
        e.right *= s;
        e.bottom *= s;
        e.left *= s;
    }
    let d = &mut b.dimensions;
    d.content.x *= s;
    d.content.y *= s;
    d.content.width *= s;
    d.content.height *= s;
    scale_edges(&mut d.padding, s);
    scale_edges(&mut d.border, s);
    scale_edges(&mut d.margin, s);

    b.style.font_size *= s;
    b.style.letter_spacing *= s;
    if let Some(lh) = b.style.line_height.as_mut() {
        *lh *= s;
    }
    if let Some(ext) = b.style.extras.as_deref_mut() {
        ext.border_radius *= s;
        for sh in &mut ext.box_shadows {
            sh.dx *= s;
            sh.dy *= s;
            sh.blur *= s;
            sh.spread *= s;
        }
        // transform = [a, b, c, d, e, f]; e/f are the px translation (a..d are the unitless
        // linear part — scale/rotation/skew — which must not be touched).
        if let Some(t) = ext.transform.as_mut() {
            t[4] *= s;
            t[5] *= s;
        }
    }
    if let Some(insets) = b.used_insets.as_mut() {
        for v in insets.iter_mut() {
            *v *= s;
        }
    }
    if let Some(margins) = b.used_margins.as_mut() {
        for v in margins.iter_mut() {
            *v *= s;
        }
    }

    for c in &mut b.children {
        scale_layout_tree(c, s);
    }
}

pub(crate) fn collect_node_rects(b: &layout::LayoutBox, out: &mut HashMap<usize, layout::Rect>) {
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
pub(crate) fn collect_used_insets(b: &layout::LayoutBox, out: &mut HashMap<usize, [f32; 4]>) {
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
pub(crate) fn collect_used_margins(b: &layout::LayoutBox, out: &mut HashMap<usize, [f32; 4]>) {
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
pub(crate) fn compute_initial_rects(
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
    style::set_viewport_metrics(vp_w as f32, vp_h as f32, scale);
    style::set_interaction_state(None, None);
    // Lay out against the LOGICAL (CSS-px) viewport, then bake the backing scale into the tree —
    // identical to the authoritative `ensure_layout` path, so the rects this seeds into the JS
    // session match what the real layout pushes (otherwise HiDPI getBoundingClientRect reads
    // would briefly see half-size boxes before the authoritative push lands).
    let vw = (vp_w as f32).max(1.0);
    let vh = (vp_h as f32).max(1.0);
    let measurer = FontMeasurer { font };
    let mut intrinsic_sizes: HashMap<dom::NodeId, (f32, f32)> = images
        .iter()
        .map(|(&id, img)| (id, (img.w as f32, img.h as f32)))
        .collect();
    collect_canvas_intrinsics(doc, &mut intrinsic_sizes);
    collect_svg_intrinsics(doc, &mut intrinsic_sizes);
    let (computed, _root_scheme_dark) = style::cascade_with_root_scheme(doc, styles, is_dark);
    let mut root =
        layout::layout_document(doc, &computed, vw, vh, &measurer, &intrinsic_sizes, None);
    // CSS px → device px, matching `ensure_layout`. `collect_node_rects` below then yields device px,
    // which the `inv` (1/scale) factor converts back to the CSS px the JS session stores.
    scale_layout_tree(&mut root, scale);
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
pub(crate) fn collect_mask_targets(
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
pub(crate) fn load_mask_source(url: &str, base: &str) -> Option<MaskSource> {
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
pub(crate) fn sniff_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(512)];
    let s = String::from_utf8_lossy(head).to_ascii_lowercase();
    s.contains("<svg")
}

/// Rasterize a [`MaskSource`] to an `out_w × out_h` coverage bitmap (RGBA, but only the ALPHA
/// channel is meaningful — it's the mask coverage). `size` picks the fit: `Stretch` fills the box
/// exactly; `Contain`/`Cover` preserve aspect ratio (the engine's SVG rasterizer already does a
/// contain-style `xMidYMid meet` fit, so for SVG we rasterize straight at the box size; for raster
/// masks we fit the source rect and centre it).
pub(crate) fn rasterize_mask_coverage(
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
pub(crate) fn paint_masked_bg(
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
pub(crate) fn collect_content_rects(b: &layout::LayoutBox, out: &mut HashMap<usize, layout::Rect>) {
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
pub(crate) fn collect_canvas_intrinsics(
    doc: &dom::Document,
    out: &mut HashMap<dom::NodeId, (f32, f32)>,
) {
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
pub(crate) fn collect_svg_intrinsics(
    doc: &dom::Document,
    out: &mut HashMap<dom::NodeId, (f32, f32)>,
) {
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
pub(crate) fn fnum(v: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

/// True if `id` is an editable text field: a text-like `<input>` (type text/search/email/url/tel/
/// password/number/none) or a `<textarea>`, and not `disabled`/`readonly`. These are the controls
/// that accept typed character input.
pub(crate) fn is_editable_text_field(doc: &dom::Document, id: dom::NodeId) -> bool {
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
pub(crate) fn editable_text_ancestor(
    doc: &dom::Document,
    node: dom::NodeId,
) -> Option<dom::NodeId> {
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
pub(crate) fn node_value(doc: &dom::Document, id: dom::NodeId) -> Option<String> {
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
pub(crate) fn checkable_target(doc: &dom::Document, node: dom::NodeId) -> Option<dom::NodeId> {
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
pub(crate) fn details_toggle_target(doc: &dom::Document, node: dom::NodeId) -> Option<dom::NodeId> {
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
pub(crate) fn find_by_attr_id(
    doc: &dom::Document,
    root: dom::NodeId,
    id: &str,
) -> Option<dom::NodeId> {
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
pub(crate) fn find_tag(doc: &dom::Document, root: dom::NodeId, tag: &str) -> Option<dom::NodeId> {
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
pub(crate) fn is_single_line_input(doc: &dom::Document, id: dom::NodeId) -> bool {
    matches!(&doc.get(id).data, dom::NodeData::Element(e) if e.tag.eq_ignore_ascii_case("input"))
}

/// Walk up from `node` to the nearest ancestor `<form>`, if any.
pub(crate) fn ancestor_form(doc: &dom::Document, node: dom::NodeId) -> Option<dom::NodeId> {
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
pub(crate) fn submit_target_form(doc: &dom::Document, node: dom::NodeId) -> Option<dom::NodeId> {
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
