use crate::*;

/// Recompute the cascade if the cache is missing or stale (DOM changed since it was built), then run
/// `f` over the cached `ComputedStyle` for `id` (`None` if `id` has no computed style — e.g. a text
/// node or out-of-range id). Keeps the borrow of `computed_cache` scoped to this call.
pub(crate) fn with_computed_style<R>(
    state: &HostState,
    id: dom::NodeId,
    f: impl FnOnce(Option<&style::ComputedStyle>) -> R,
) -> R {
    let version = state.dom_version.get();
    {
        let mut cache = state.computed_cache.borrow_mut();
        let fresh = matches!(&*cache, Some((v, _)) if *v == version);
        if !fresh {
            let doc = state.doc.borrow();
            let sheets = collect_author_sheets(&doc, &state.fetcher, &state.page_url.borrow());
            let map = style::cascade(&doc, &sheets);
            *cache = Some((version, map));
        }
    }
    let cache = state.computed_cache.borrow();
    let map = cache.as_ref().map(|(_, m)| m);
    if let Some(m) = map {
        if let Some(cs) = m.get(&id) {
            return f(Some(cs));
        }
        // Not in the main document cascade — it may live in an <iframe> facade document. Cascade that
        // detached subtree on its own, with the iframe's size as the viewport so the frame's @media
        // queries get their own context (re-resolved live as the iframe resizes).
        let doc = state.doc.borrow();
        if let Some((facade_root, iframe_id)) = find_facade_root(&doc, id) {
            // A `display: none` iframe isn't rendered, so its document has no boxes — getComputedStyle
            // returns the empty (no-value) style for its elements.
            if m.get(&iframe_id).map(|cs| cs.display_none).unwrap_or(false) {
                return f(None);
            }
            let attr_dim = |name: &str| -> Option<f32> {
                if let dom::NodeData::Element(e) = &doc.get(iframe_id).data {
                    e.attrs.get(name).and_then(|v| v.trim().parse::<f32>().ok())
                } else {
                    None
                }
            };
            // Content width is resolved recursively (a nested iframe's % width is relative to the
            // outer iframe's content width); height uses the simpler CSS-or-attr-or-default form.
            let iw = iframe_content_width(state, &doc, m, iframe_id, 0);
            let ih = m
                .get(&iframe_id)
                .and_then(|cs| cs.height)
                .or_else(|| attr_dim("height"))
                .unwrap_or(150.0);
            let sheets = collect_facade_sheets(&doc, facade_root, &state.fetcher);
            let (sw, sh, sd) = style::viewport_metrics();
            style::set_viewport_metrics(iw, ih, sd);
            let mut submap = style::cascade_subtree(&doc, facade_root, &sheets);
            style::set_viewport_metrics(sw, sh, sd);
            // Resolve percentage widths against the iframe content width (top-down), so
            // getComputedStyle reports their used px value (the frame has no real layout pass).
            resolve_facade_widths(&doc, &mut submap, facade_root, iw);
            return f(submap.get(&id));
        }
    }
    f(None)
}

/// Walk up from `id` to the nearest `<iframe>` facade-document root (its body carries a
/// `data-frame-host` attribute = the host iframe's node id). Returns `(facade_root, iframe_node)`.
pub(crate) fn find_facade_root(
    doc: &dom::Document,
    id: dom::NodeId,
) -> Option<(dom::NodeId, dom::NodeId)> {
    let mut cur = Some(id);
    while let Some(c) = cur {
        if c.0 < doc.len() {
            if let dom::NodeData::Element(e) = &doc.get(c).data {
                if let Some(v) = e.attrs.get("data-frame-host") {
                    if let Ok(raw) = v.trim().parse::<usize>() {
                        return Some((c, dom::NodeId(raw)));
                    }
                }
            }
        }
        cur = doc.get(c).parent;
    }
    None
}

/// The content width of an `<iframe>` in CSS px: explicit CSS width, else the `width` HTML attribute,
/// else — for a nested iframe whose width is a percentage — resolved by cascading the *outer* iframe
/// facade (recursing through outer frames), else the conventional 300px default.
pub(crate) fn iframe_content_width(
    state: &HostState,
    doc: &dom::Document,
    map: &HashMap<dom::NodeId, style::ComputedStyle>,
    iframe_id: dom::NodeId,
    depth: u32,
) -> f32 {
    if depth > 8 {
        return 300.0;
    }
    if let Some(w) = map.get(&iframe_id).and_then(|c| c.width) {
        return w;
    }
    if let dom::NodeData::Element(e) = &doc.get(iframe_id).data {
        if let Some(w) = e
            .attrs
            .get("width")
            .and_then(|v| v.trim().parse::<f32>().ok())
        {
            return w;
        }
    }
    // Not sized directly: the iframe lives inside an outer iframe's facade (a nested frame). Cascade
    // that outer facade against the outer iframe's content width, resolve widths, and read this
    // iframe's used width there.
    if let Some((outer_root, outer_iframe)) = find_facade_root(doc, iframe_id) {
        let outer_w = iframe_content_width(state, doc, map, outer_iframe, depth + 1);
        let sheets = collect_facade_sheets(doc, outer_root, &state.fetcher);
        let (sw, sh, sd) = style::viewport_metrics();
        style::set_viewport_metrics(outer_w, 150.0, sd);
        let mut submap = style::cascade_subtree(doc, outer_root, &sheets);
        style::set_viewport_metrics(sw, sh, sd);
        resolve_facade_widths(doc, &mut submap, outer_root, outer_w);
        if let Some(w) = submap.get(&iframe_id).and_then(|c| c.width) {
            return w;
        }
    }
    300.0
}

/// Resolve percentage / auto block widths down a facade subtree so getComputedStyle reports a used
/// px width (the frame has no layout pass). `avail_width` is the containing block's content width.
pub(crate) fn resolve_facade_widths(
    doc: &dom::Document,
    submap: &mut HashMap<dom::NodeId, style::ComputedStyle>,
    id: dom::NodeId,
    avail_width: f32,
) {
    let content_w = if let Some(cs) = submap.get_mut(&id) {
        match cs
            .width
            .or_else(|| cs.width_pct.map(|p| (avail_width * p).max(0.0)))
        {
            Some(w) => {
                cs.width = Some(w);
                (w - cs.padding.left - cs.padding.right - cs.border.left - cs.border.right).max(0.0)
            }
            None => (avail_width
                - cs.margin.left
                - cs.margin.right
                - cs.padding.left
                - cs.padding.right
                - cs.border.left
                - cs.border.right)
                .max(0.0),
        }
    } else {
        avail_width
    };
    let children: Vec<dom::NodeId> = doc.get(id).children.clone();
    for child in children {
        resolve_facade_widths(doc, submap, child, content_w);
    }
}

/// The author CSS inside an iframe facade subtree (its `<style>` text + `<link>` CSS), as one sheet.
pub(crate) fn collect_facade_sheets(
    doc: &dom::Document,
    root: dom::NodeId,
    fetcher: &Rc<dyn Fn(&str) -> Option<(String, String)>>,
) -> Vec<css::Stylesheet> {
    fn walk(
        doc: &dom::Document,
        id: dom::NodeId,
        out: &mut String,
        fetcher: &Rc<dyn Fn(&str) -> Option<(String, String)>>,
    ) {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag.eq_ignore_ascii_case("style") {
                out.push_str(&text_content(doc, id));
                out.push('\n');
                return;
            }
            if e.tag.eq_ignore_ascii_case("link") {
                if let Some(href) = e.attrs.get("href") {
                    if let Some(css) = fetch_link_css(href, fetcher) {
                        out.push_str(&css);
                        out.push('\n');
                    }
                }
            }
        }
        for &child in &doc.get(id).children {
            walk(doc, child, out, fetcher);
        }
    }
    let mut css_src = String::new();
    walk(doc, root, &mut css_src, fetcher);
    if css_src.trim().is_empty() {
        Vec::new()
    } else {
        vec![css::parse(&css_src)]
    }
}

/// Like [`with_computed_style`] but exposes the whole cascade `map` plus the live `Document`, so a
/// caller can read several elements at once (e.g. an element and its containing block, for the
/// CSSOM resolved-value of insets). Recomputes the cascade if stale, same as `with_computed_style`.
pub(crate) fn with_cascade_map<R>(
    state: &HostState,
    f: impl FnOnce(&dom::Document, &HashMap<dom::NodeId, style::ComputedStyle>) -> R,
) -> R {
    let version = state.dom_version.get();
    {
        let mut cache = state.computed_cache.borrow_mut();
        let fresh = matches!(&*cache, Some((v, _)) if *v == version);
        if !fresh {
            let doc = state.doc.borrow();
            let sheets = collect_author_sheets(&doc, &state.fetcher, &state.page_url.borrow());
            let map = style::cascade(&doc, &sheets);
            *cache = Some((version, map));
        }
    }
    let cache = state.computed_cache.borrow();
    let map = cache
        .as_ref()
        .map(|(_, m)| m)
        .expect("cascade just populated");
    let doc = state.doc.borrow();
    f(&doc, map)
}

/// Compute the requested pseudo-element's `ComputedStyle` for node `id` and run `f` over it
/// (`None` when `id` isn't an element). `pseudo_key` is the canonical key from
/// [`style::parse_gcs_pseudo`] (`"before"`, `"marker"`, `"highlight(x)"`, …). Recomputes the
/// element cascade if stale (same freshness check as [`with_computed_style`]), then derives the
/// pseudo style on demand from the element's cascaded style + the author sheets.
pub(crate) fn with_pseudo_style<R>(
    state: &HostState,
    id: dom::NodeId,
    pseudo_key: &str,
    f: impl FnOnce(Option<&style::ComputedStyle>) -> R,
) -> R {
    let version = state.dom_version.get();
    {
        let mut cache = state.computed_cache.borrow_mut();
        let fresh = matches!(&*cache, Some((v, _)) if *v == version);
        if !fresh {
            let doc = state.doc.borrow();
            let sheets = collect_author_sheets(&doc, &state.fetcher, &state.page_url.borrow());
            let map = style::cascade(&doc, &sheets);
            *cache = Some((version, map));
        }
    }
    let cache = state.computed_cache.borrow();
    let map = cache
        .as_ref()
        .map(|(_, m)| m)
        .expect("cascade just populated");
    let doc = state.doc.borrow();
    let element_style = map.get(&id);
    let pseudo = element_style.and_then(|es| {
        let sheets = collect_author_sheets(&doc, &state.fetcher, &state.page_url.borrow());
        style::compute_pseudo_style(&doc, &sheets, id, es, pseudo_key)
    });
    f(pseudo.as_ref())
}

/// The content-box and padding-box extents (width, height) of an element's box, derived from its
/// computed style. Standard box-sizing: `width`/`height` are the content box; the padding box adds
/// the padding edges. Used as the percentage basis / containing-block extent when resolving insets.
pub(crate) fn box_extents(cs: &style::ComputedStyle) -> ((f32, f32), (f32, f32)) {
    let cw = cs.width.unwrap_or(0.0);
    let ch = cs.height.unwrap_or(0.0);
    let pw = cw + cs.padding.left + cs.padding.right;
    let ph = ch + cs.padding.top + cs.padding.bottom;
    ((cw, ch), (pw, ph))
}

/// Compute the CSSOM resolved value of inset `side` (top/right/bottom/left) for node `id`, given a
/// freshly-cascaded `map`. Walks the DOM to find the element's containing block and computes the
/// percentage basis from that block's specified geometry (no layout needed for the cases the WPT
/// inset tests exercise — the containers have explicit px sizes). Returns `None` for non-elements.
pub(crate) fn resolved_inset_value(
    doc: &dom::Document,
    map: &HashMap<dom::NodeId, style::ComputedStyle>,
    id: dom::NodeId,
    side: style::EdgeSide,
    used: Option<f32>,
) -> Option<String> {
    let cs = map.get(&id)?;
    // A box-less element (display:none, or an ancestor is) has no used value → computed value.
    let box_less = cs.display == style::Display::None || ancestor_display_none(doc, map, id);
    if box_less || cs.position == style::Position::Static {
        return Some(cs.resolved_inset(side, true, f32::NAN));
    }

    // Absolute / fixed with BOTH opposite insets `auto`: the box sits at its static position, whose
    // used inset value needs layout. We compute it synchronously from specified geometry (the WPT
    // containers have explicit px sizes) — the engine-pushed used px (`used`) is a fallback for cases
    // where the static position can't be derived from specified geometry alone.
    if matches!(
        cs.position,
        style::Position::Absolute | style::Position::Fixed
    ) {
        let (spec, opposite) = match side {
            style::EdgeSide::Top => (cs.top_spec, cs.bottom_spec),
            style::EdgeSide::Bottom => (cs.bottom_spec, cs.top_spec),
            style::EdgeSide::Left => (cs.left_spec, cs.right_spec),
            style::EdgeSide::Right => (cs.right_spec, cs.left_spec),
            style::EdgeSide::All => (cs.top_spec, cs.bottom_spec),
        };
        let both_auto =
            matches!(spec, style::InsetValue::Auto) && matches!(opposite, style::InsetValue::Auto);
        if both_auto {
            // Prefer the engine-pushed used value (from real layout) when present; otherwise derive
            // the static position synchronously from specified geometry (covers the synchronous
            // mutate-then-read pattern the CSSOM tests use, before any layout/push has run).
            if let Some(px_val) = used {
                return Some(style::serialize_px(px_val));
            }
            let want_height = matches!(side, style::EdgeSide::Top | style::EdgeSide::Bottom);
            let kind = if cs.position == style::Position::Absolute {
                ContainingBlock::PositionedPadding
            } else {
                ContainingBlock::TransformedPadding
            };
            let cb_node = containing_block_node(doc, map, id, kind);
            let static_off = static_position_offset(doc, map, id, cb_node, want_height);
            let cb_extent = containing_block_extent(doc, map, id, kind, want_height);
            let mb_size = if want_height {
                cs.height.unwrap_or(0.0)
                    + cs.padding.top
                    + cs.padding.bottom
                    + cs.border.top
                    + cs.border.bottom
                    + cs.margin.top
                    + cs.margin.bottom
            } else {
                cs.width.unwrap_or(0.0)
                    + cs.padding.left
                    + cs.padding.right
                    + cs.border.left
                    + cs.border.right
                    + cs.margin.left
                    + cs.margin.right
            };
            // Map the static position to the *containing block's* writing mode: a physical side that
            // is the cb's block- or inline-START takes the static offset directly; the opposite side
            // measures from the far edge (cb extent − offset − the box's margin-box size).
            let (block_start, inline_start) = cb_node
                .and_then(|n| map.get(&n))
                .map(|c| c.writing_mode.start_edges(c.direction))
                .unwrap_or((style::EdgeSide::Top, style::EdgeSide::Left));
            let used_val = if side == block_start || side == inline_start {
                static_off
            } else {
                cb_extent - static_off - mb_size
            };
            return Some(style::serialize_px(used_val));
        }
    }

    // Find the containing block and the relevant axis extent (height for top/bottom, width for
    // left/right). For in-flow / relative / sticky the cb is the parent's *content* box; for
    // absolute the nearest positioned ancestor's *padding* box; for fixed the nearest transformed
    // ancestor's padding box (the viewport otherwise — approximated by the document element).
    let want_height = matches!(side, style::EdgeSide::Top | style::EdgeSide::Bottom);
    let basis = match cs.position {
        style::Position::Relative => {
            containing_block_extent(doc, map, id, ContainingBlock::ParentContent, want_height)
        }
        // Sticky insets resolve against the nearest scrollport (overflow != visible) ancestor's
        // content box, not the parent — per the CSSOM sticky resolved-value rule.
        style::Position::Sticky => {
            containing_block_extent(doc, map, id, ContainingBlock::StickyScrollport, want_height)
        }
        style::Position::Absolute => containing_block_extent(
            doc,
            map,
            id,
            ContainingBlock::PositionedPadding,
            want_height,
        ),
        style::Position::Fixed => containing_block_extent(
            doc,
            map,
            id,
            ContainingBlock::TransformedPadding,
            want_height,
        ),
        style::Position::Static => f32::NAN,
    };
    Some(cs.resolved_inset(side, false, basis))
}

/// Which ancestor establishes the containing block, and which box of it forms the basis.
#[derive(Clone, Copy)]
pub(crate) enum ContainingBlock {
    /// Parent element's content box (in-flow / relative / sticky).
    ParentContent,
    /// Nearest positioned ancestor's padding box (absolute).
    PositionedPadding,
    /// Nearest ancestor with a transform's padding box, else the document element (fixed).
    TransformedPadding,
    /// Nearest scroll-container (scrollport) ancestor's content box — the box a `position: sticky`
    /// element's inset percentages resolve against (the nearest ancestor with `overflow != visible`,
    /// else the parent). CSSOM resolved value for sticky insets.
    StickyScrollport,
}

/// Find the containing-block element for `id` per `kind` (the abspos/fixed cb walk), returning its
/// node id, or `None` if none is found (the cb is the viewport / initial containing block).
pub(crate) fn containing_block_node(
    doc: &dom::Document,
    map: &HashMap<dom::NodeId, style::ComputedStyle>,
    id: dom::NodeId,
    kind: ContainingBlock,
) -> Option<dom::NodeId> {
    let mut cur = doc.get(id).parent;
    match kind {
        ContainingBlock::ParentContent => cur,
        ContainingBlock::PositionedPadding => {
            while let Some(a) = cur {
                if map
                    .get(&a)
                    .map(|acs| acs.position != style::Position::Static)
                    .unwrap_or(false)
                {
                    return Some(a);
                }
                cur = doc.get(a).parent;
            }
            None
        }
        ContainingBlock::TransformedPadding => {
            while let Some(a) = cur {
                if map
                    .get(&a)
                    .map(|acs| acs.transform.is_some())
                    .unwrap_or(false)
                {
                    return Some(a);
                }
                cur = doc.get(a).parent;
            }
            None
        }
        ContainingBlock::StickyScrollport => {
            while let Some(a) = cur {
                if map
                    .get(&a)
                    .map(|acs| acs.overflow_scrollport)
                    .unwrap_or(false)
                {
                    return Some(a);
                }
                cur = doc.get(a).parent;
            }
            None
        }
    }
}

/// The static-position offset of out-of-flow `id`'s margin box from its containing block's
/// content-area start edge, on one axis (`want_height` → vertical/top, else horizontal/left), in px.
///
/// The hypothetical in-flow position of `id` is the start of its parent's content box; the parent's
/// content box is in turn offset from the containing block (`cb`, exclusive) by each intervening
/// ancestor's margin + border + padding start edge, plus the cb's own padding start edge. Computed
/// from specified geometry (the WPT inset containers all have explicit px sizes), so it matches real
/// layout for those tests without a layout pass. `cb` = `None` means the containing block is the
/// initial containing block (the document root), so we accumulate up to (and including) the root.
pub(crate) fn static_position_offset(
    doc: &dom::Document,
    map: &HashMap<dom::NodeId, style::ComputedStyle>,
    id: dom::NodeId,
    cb: Option<dom::NodeId>,
    want_height: bool,
) -> f32 {
    let start_edge = |e: &style::Edges| if want_height { e.top } else { e.left };
    let mut offset = 0.0;
    // The cb's own padding start edge: its content box begins after its padding.
    if let Some(cbid) = cb {
        if let Some(cbcs) = map.get(&cbid) {
            offset += start_edge(&cbcs.padding);
        }
    }
    // Each ancestor strictly between `id` and the cb contributes its full start margin+border+padding.
    let mut cur = doc.get(id).parent;
    while let Some(a) = cur {
        if Some(a) == cb {
            break;
        }
        if let Some(acs) = map.get(&a) {
            offset += start_edge(&acs.margin) + start_edge(&acs.border) + start_edge(&acs.padding);
        }
        cur = doc.get(a).parent;
    }
    offset
}

/// Walk up from `id` to find its containing block per `kind`, returning the requested axis extent
/// (height when `want_height`, else width). Falls back to `0.0` if no suitable ancestor is found.
pub(crate) fn containing_block_extent(
    doc: &dom::Document,
    map: &HashMap<dom::NodeId, style::ComputedStyle>,
    id: dom::NodeId,
    kind: ContainingBlock,
    want_height: bool,
) -> f32 {
    let pick = |extents: ((f32, f32), (f32, f32)), padding: bool| {
        let (content, pad) = extents;
        let (w, h) = if padding { pad } else { content };
        if want_height {
            h
        } else {
            w
        }
    };
    let mut cur = doc.get(id).parent;
    match kind {
        ContainingBlock::ParentContent => {
            if let Some(p) = cur {
                if let Some(pcs) = map.get(&p) {
                    return pick(box_extents(pcs), false);
                }
            }
            0.0
        }
        ContainingBlock::PositionedPadding => {
            while let Some(a) = cur {
                if let Some(acs) = map.get(&a) {
                    if acs.position != style::Position::Static {
                        return pick(box_extents(acs), true);
                    }
                }
                cur = doc.get(a).parent;
            }
            0.0
        }
        ContainingBlock::TransformedPadding => {
            while let Some(a) = cur {
                if let Some(acs) = map.get(&a) {
                    if acs.transform.is_some() {
                        return pick(box_extents(acs), true);
                    }
                }
                cur = doc.get(a).parent;
            }
            0.0
        }
        ContainingBlock::StickyScrollport => {
            // Nearest scrollport ancestor's content box; fall back to the parent's content box when
            // there's no scroll container (the box's normal containing block).
            let mut fallback = None;
            while let Some(a) = cur {
                if let Some(acs) = map.get(&a) {
                    if fallback.is_none() {
                        fallback = Some(pick(box_extents(acs), false));
                    }
                    if acs.overflow_scrollport {
                        return pick(box_extents(acs), false);
                    }
                }
                cur = doc.get(a).parent;
            }
            fallback.unwrap_or(0.0)
        }
    }
}

/// True if any ancestor of `id` has `display: none` (so `id` has no rendered box even if its own
/// `display` is not `none`).
pub(crate) fn ancestor_display_none(
    doc: &dom::Document,
    map: &HashMap<dom::NodeId, style::ComputedStyle>,
    id: dom::NodeId,
) -> bool {
    let mut cur = doc.get(id).parent;
    while let Some(a) = cur {
        if let Some(acs) = map.get(&a) {
            if acs.display == style::Display::None {
                return true;
            }
        }
        cur = doc.get(a).parent;
    }
    false
}

fn resolved_grid_tracks(
    tracks: &[style::TrackSize],
    available: f32,
    gap: f32,
    item_count: usize,
    columns: bool,
) -> String {
    let mut expanded = Vec::new();
    for track in tracks {
        match *track {
            style::TrackSize::AutoRepeatFill(size) | style::TrackSize::AutoRepeatFit(size) => {
                let repetitions = (available + gap).max(0.0) / (size + gap).max(f32::EPSILON);
                // In an indefinite inline axis, auto-fit uses the largest whole number that fits;
                // the block axis and auto-fill cover the minimum size with a final repetition.
                let count = if columns && matches!(track, style::TrackSize::AutoRepeatFit(_)) {
                    repetitions.floor()
                } else {
                    repetitions.ceil()
                } as usize;
                let count = count.max(1);
                for index in 0..count {
                    let used = if matches!(track, style::TrackSize::AutoRepeatFit(_))
                        && index >= item_count
                    {
                        0.0
                    } else {
                        size
                    };
                    expanded.push(style::TrackSize::Px(used));
                }
            }
            other => expanded.push(other),
        }
    }
    let tracks = expanded.as_slice();
    let track_space = (available - gap * tracks.len().saturating_sub(1) as f32).max(0.0);
    let mut fixed = 0.0;
    let mut fr_total = 0.0;
    let mut auto_count = 0usize;
    for track in tracks {
        match track {
            style::TrackSize::Px(value) => fixed += value,
            style::TrackSize::Pct(percent) => fixed += track_space * (percent / 100.0),
            style::TrackSize::Fr(fraction) => fr_total += fraction,
            style::TrackSize::Auto => auto_count += 1,
            style::TrackSize::AutoRepeatFill(_) | style::TrackSize::AutoRepeatFit(_) => {}
        }
    }
    let remaining = (track_space - fixed).max(0.0);
    let auto_size = if fr_total == 0.0 && auto_count > 0 {
        remaining / auto_count as f32
    } else {
        0.0
    };
    let fr_size = if fr_total > 0.0 {
        (remaining - auto_size * auto_count as f32).max(0.0) / fr_total
    } else {
        0.0
    };
    tracks
        .iter()
        .map(|track| {
            let value = match track {
                style::TrackSize::Px(value) => *value,
                style::TrackSize::Pct(percent) => track_space * (percent / 100.0),
                style::TrackSize::Fr(fraction) => fr_size * fraction,
                style::TrackSize::Auto => auto_size,
                style::TrackSize::AutoRepeatFill(_) | style::TrackSize::AutoRepeatFit(_) => 0.0,
            };
            style::serialize_px(value)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `__computedStyleProp(id, name) -> string` — the computed value of CSS property `name` (kebab,
/// lowercased by JS) for node `id`, or "" if there's no computed style (non-element / unknown id) or
/// the property isn't tracked.
pub(crate) fn prim_computed_style_prop(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let name = arg_str(scope, &args, 1);
    let pseudo = style::parse_gcs_pseudo(&arg_str(scope, &args, 2));
    // A 4th truthy arg requests the *computed* value (computedStyleMap) rather than the resolved
    // value (getComputedStyle) — they differ for colors the forced-colors override replaced.
    let raw = args.get(3).is_true();
    let get = |cs: &style::ComputedStyle| {
        if raw {
            cs.get_property_computed(&name)
        } else {
            cs.get_property(&name)
        }
    };
    let state = host_state(scope);
    let value = match (node, &pseudo) {
        (None, _) | (_, style::GcsPseudo::Invalid) => String::new(),
        (Some(n), style::GcsPseudo::Pseudo(key)) => {
            with_pseudo_style(&state, n, key, |cs| cs.map(get).unwrap_or_default())
        }
        (Some(n), style::GcsPseudo::Element) => {
            let grid_axis_is_columns = match name.as_str() {
                "grid-template-columns" => Some(true),
                "grid-template-rows" => Some(false),
                _ => None,
            };
            // Inset longhands need the CSSOM resolved-value algorithm (position + containing
            // block), which reads more than one element's style — handle them via the cascade map.
            let inset_side = match name.as_str() {
                "top" => Some(style::EdgeSide::Top),
                "right" => Some(style::EdgeSide::Right),
                "bottom" => Some(style::EdgeSide::Bottom),
                "left" => Some(style::EdgeSide::Left),
                _ => None,
            };
            // Margin longhands report the CSSOM *used* value: the engine pushes resolved margins
            // (including `auto` centering / over-constrained boxes) keyed by node id.
            let margin_idx = match name.as_str() {
                "margin-top" => Some(0usize),
                "margin-right" => Some(1),
                "margin-bottom" => Some(2),
                "margin-left" => Some(3),
                _ => None,
            };
            // width/height report the CSSOM *used* (content-box px) value when the element has a box
            // and the property applies — e.g. a percentage width the cascade couldn't resolve.
            let size_is_width = match name.as_str() {
                "width" => Some(true),
                "height" => Some(false),
                _ => None,
            };
            // min-width / min-height: auto resolves to 0px, except a box with a preferred aspect
            // ratio or a flex/grid item keeps `auto`; a box-less element (no layout box) is 0px.
            if let Some(columns) = grid_axis_is_columns {
                crate::forced_layout::ensure_layout_fresh(&state);
                let item_count = state.doc.borrow().get(n).children.len();
                with_computed_style(&state, n, |cs| {
                    let Some(cs) = cs else {
                        return String::new();
                    };
                    if !matches!(
                        cs.display,
                        style::Display::Grid | style::Display::InlineGrid
                    ) {
                        return cs.get_property(&name);
                    }
                    let tracks = if columns {
                        &cs.grid_template_columns
                    } else {
                        &cs.grid_template_rows
                    };
                    if tracks.is_empty() {
                        return "none".to_string();
                    }
                    let Some(&(_, _, width, height)) = state.layout_rects.borrow().get(&n.0) else {
                        return cs.get_property(&name);
                    };
                    let available = if columns {
                        width
                            - cs.border.left
                            - cs.border.right
                            - cs.padding.left
                            - cs.padding.right
                    } else {
                        height
                            - cs.border.top
                            - cs.border.bottom
                            - cs.padding.top
                            - cs.padding.bottom
                    }
                    .max(0.0);
                    let gap = if columns { cs.column_gap } else { cs.row_gap };
                    resolved_grid_tracks(tracks, available, gap, item_count, columns)
                })
            } else if matches!(name.as_str(), "min-width" | "min-height") {
                with_cascade_map(&state, |doc, map| {
                    let cs = match map.get(&n) {
                        Some(c) => c,
                        None => return String::new(),
                    };
                    let computed = cs.get_property(&name);
                    if computed != "auto" {
                        return computed;
                    }
                    // A box-less element (display:none, or inside one) generates no box → 0px.
                    if cs.display_none || ancestor_display_none(doc, map, n) {
                        return "0px".to_string();
                    }
                    let parent_flex_grid = doc
                        .get(n)
                        .parent
                        .and_then(|p| map.get(&p))
                        .map(|p| {
                            matches!(
                                p.display,
                                style::Display::Flex
                                    | style::Display::InlineFlex
                                    | style::Display::Grid
                                    | style::Display::InlineGrid
                            )
                        })
                        .unwrap_or(false);
                    if cs.aspect_ratio_set || parent_flex_grid {
                        "auto".to_string()
                    } else {
                        "0px".to_string()
                    }
                })
            } else if let Some(is_width) = size_is_width {
                with_computed_style(&state, n, |cs| {
                    let cs = match cs {
                        Some(c) => c,
                        None => return String::new(),
                    };
                    let computed = cs.get_property(&name);
                    // A specified length is already the used value (and stays fresh between layouts);
                    // only resolve `auto`/percentage via the laid-out box. width/height don't apply to
                    // non-replaced inline boxes or display:none, where the computed value is reported.
                    if computed != "auto"
                        || cs.display_none
                        || matches!(cs.display, style::Display::Inline)
                    {
                        return computed;
                    }
                    match state.layout_rects.borrow().get(&n.0) {
                        Some(&(_, _, w, h)) => {
                            let (b0, b1, p0, p1) = if is_width {
                                (
                                    cs.border.left,
                                    cs.border.right,
                                    cs.padding.left,
                                    cs.padding.right,
                                )
                            } else {
                                (
                                    cs.border.top,
                                    cs.border.bottom,
                                    cs.padding.top,
                                    cs.padding.bottom,
                                )
                            };
                            let border_box = if is_width { w } else { h };
                            style::serialize_px((border_box - b0 - b1 - p0 - p1).max(0.0))
                        }
                        None => computed,
                    }
                })
            } else if let Some(idx) = margin_idx {
                // Only an `auto` margin needs the engine's resolved used value; a specified margin's
                // computed value is always current (and not stale between layouts). Falls back to the
                // computed value if the engine hasn't pushed a used margin yet.
                with_computed_style(&state, n, |cs| match cs {
                    Some(cs) if cs.margin_auto[idx] => state
                        .used_margins
                        .borrow()
                        .get(&n.0)
                        .map(|&(t, r, b, l)| style::serialize_px([t, r, b, l][idx]))
                        .unwrap_or_else(|| cs.get_property(&name)),
                    Some(cs) => cs.get_property(&name),
                    None => String::new(),
                })
            } else {
                match inset_side {
                    Some(side) => {
                        // The engine pushed this box's used inset values (px) keyed by node id; pick this
                        // side. Used by the resolved-value algorithm for cases that need real layout
                        // (absolute/fixed static position when both opposite insets are `auto`).
                        let used =
                            state
                                .used_insets
                                .borrow()
                                .get(&n.0)
                                .map(|&(t, r, b, l)| match side {
                                    style::EdgeSide::Top => t,
                                    style::EdgeSide::Right => r,
                                    style::EdgeSide::Bottom => b,
                                    style::EdgeSide::Left => l,
                                    style::EdgeSide::All => t,
                                });
                        with_cascade_map(&state, |doc, map| {
                            resolved_inset_value(doc, map, n, side, used).unwrap_or_default()
                        })
                    }
                    None => with_computed_style(&state, n, |cs| cs.map(get).unwrap_or_default()),
                }
            }
        }
    };
    let s = js_str(scope, &value);
    rv.set(s);
}

/// The computed-style property names for one element: the standard tracked longhands plus this
/// element's resolved custom properties (`--name`), which `getComputedStyle(el)` enumerates per
/// CSSOM in lexicographical order — with vendor-prefixed / custom names (leading `-`) sorted last.
pub(crate) fn computed_names_with_custom(cs: &style::ComputedStyle) -> Vec<String> {
    let mut names: Vec<String> = cs.property_names().iter().map(|s| s.to_string()).collect();
    names.extend(cs.custom_props.keys().cloned());
    names.sort_by(|a, b| {
        // A leading `-` (vendor prefix / `--custom`) sorts after unprefixed; then lexicographic.
        a.starts_with('-')
            .cmp(&b.starts_with('-'))
            .then_with(|| a.cmp(b))
    });
    names
}

/// `__computedStyleNames(id) -> [name...]` — the property names with non-empty computed values for
/// node `id` (backs `length`/`item(i)`/index access/iteration). Empty array for non-elements.
pub(crate) fn prim_computed_style_names(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let pseudo = style::parse_gcs_pseudo(&arg_str(scope, &args, 1));
    let state = host_state(scope);
    let names: Vec<String> = match (node, &pseudo) {
        (None, _) | (_, style::GcsPseudo::Invalid) => Vec::new(),
        (Some(n), style::GcsPseudo::Pseudo(key)) => with_pseudo_style(&state, n, key, |cs| {
            cs.map(computed_names_with_custom).unwrap_or_default()
        }),
        (Some(n), style::GcsPseudo::Element) => with_computed_style(&state, n, |cs| {
            cs.map(computed_names_with_custom).unwrap_or_default()
        }),
    };
    let arr = js_str_array(scope, &names);
    rv.set(arr);
}

/// `__setAttr(id, name, val)`
pub(crate) fn prim_set_attr(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let name = arg_str(scope, &args, 1);
    let value = arg_str(scope, &args, 2);
    let state = host_state(scope);
    if let Some(n) = node {
        state.bump_dom_version(); // invalidate getComputedStyle cache
        let old = if state.observers_active.get() {
            // Capture the old value BEFORE overwriting (for `attributeOldValue`).
            match &state.doc.borrow().get(n).data {
                dom::NodeData::Element(e) => e.attrs.get(&name).cloned(),
                _ => None,
            }
        } else {
            None
        };
        if let dom::NodeData::Element(e) = &mut state.doc.borrow_mut().get_mut(n).data {
            e.attrs.insert(name.clone(), value);
        }
        state.record_mutation(MutationRec {
            kind: "attributes",
            target: n,
            attr_name: Some(name),
            old_value: old,
            added: Vec::new(),
            removed: Vec::new(),
        });
    }
}

/// `__removeAttr(id, name)`
pub(crate) fn prim_remove_attr(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let name = arg_str(scope, &args, 1);
    let state = host_state(scope);
    if let Some(n) = node {
        state.bump_dom_version(); // invalidate getComputedStyle cache
        let old = if state.observers_active.get() {
            match &state.doc.borrow().get(n).data {
                dom::NodeData::Element(e) => e.attrs.get(&name).cloned(),
                _ => None,
            }
        } else {
            None
        };
        if let dom::NodeData::Element(e) = &mut state.doc.borrow_mut().get_mut(n).data {
            // shift_remove preserves the insertion order of the remaining attributes (swap_remove,
            // the `remove` alias on IndexMap, would not — the DOM exposes attributes in order).
            e.attrs.shift_remove(&name);
        }
        state.record_mutation(MutationRec {
            kind: "attributes",
            target: n,
            attr_name: Some(name),
            old_value: old,
            added: Vec::new(),
            removed: Vec::new(),
        });
    }
}

/// `__attrNames(id) -> [name...]`
pub(crate) fn prim_attr_names(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let names: Vec<String> = node
        .map(|n| match &state.doc.borrow().get(n).data {
            dom::NodeData::Element(e) => e.attrs.keys().cloned().collect(),
            _ => Vec::new(),
        })
        .unwrap_or_default();
    let arr = js_str_array(scope, &names);
    rv.set(arr);
}
