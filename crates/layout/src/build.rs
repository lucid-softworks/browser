use crate::*;
use std::collections::HashMap;

// ---------------------------------------------------------------------------------------------
// Box-tree construction
// ---------------------------------------------------------------------------------------------

/// Tags whose subtrees are never rendered (metadata / scripting).
pub(crate) fn is_non_rendered_tag(tag: &str) -> bool {
    matches!(
        tag.to_ascii_lowercase().as_str(),
        "script" | "style" | "head" | "title" | "noscript" | "template" | "meta" | "link"
    )
}

/// The default value of a `<textarea>`: its descendant text content, with a single leading newline
/// stripped (per the HTML textarea parsing rule). Used when no live `value` has been set.
pub(crate) fn textarea_text_content(doc: &dom::Document, id: dom::NodeId) -> String {
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
    gather(doc, id, &mut s);
    s.strip_prefix("\r\n")
        .or_else(|| s.strip_prefix('\n'))
        .map(str::to_string)
        .unwrap_or(s)
}

/// The text a form control (`<input>` / `<textarea>`) should render inside its box, or `None`
/// if `el` isn't such a control. Returns `Some(String)` (possibly empty → a styled-but-empty box):
///   * `<textarea>`: its live `value` attribute (or empty);
///   * text-like `<input>` (text/search/email/url/tel/password/number/no-type): its `value`
///     (with `type=password` masked to bullets), else its `placeholder`, else empty;
///   * `<input type=submit|button|reset>`: its `value` as the button label (defaulting to a
///     conventional label when absent);
///   * other input types (checkbox/radio/hidden/file/image/color/range/date…): `None` (no text).
pub(crate) fn input_display_text(el: &dom::ElementData, textarea_default: &str) -> Option<String> {
    let attr = |name: &str| el.attrs.get(name).map(|s| s.as_str());
    if el.tag.eq_ignore_ascii_case("textarea") {
        // A textarea's value is its current `value` (set via JS) or, failing that, its text content
        // (the default value) — it has no `value` content attribute, so the text node is the source.
        return Some(
            attr("value")
                .map(str::to_string)
                .unwrap_or_else(|| textarea_default.to_string()),
        );
    }
    if !el.tag.eq_ignore_ascii_case("input") {
        return None;
    }
    let ty = attr("type").unwrap_or("").trim().to_ascii_lowercase();
    let text_like = matches!(
        ty.as_str(),
        "" | "text" | "search" | "email" | "url" | "tel" | "password" | "number"
    );
    if text_like {
        let value = attr("value").unwrap_or("");
        if !value.is_empty() {
            if ty == "password" {
                return Some("\u{2022}".repeat(value.chars().count()));
            }
            return Some(value.to_string());
        }
        return Some(attr("placeholder").unwrap_or("").to_string());
    }
    if matches!(ty.as_str(), "submit" | "button" | "reset") {
        let default = match ty.as_str() {
            "submit" => "Submit",
            "reset" => "Reset",
            _ => "",
        };
        return Some(attr("value").unwrap_or(default).to_string());
    }
    // Date/time pickers: a bordered field showing the value, or a format placeholder. We don't
    // build a real picker — just visible text so the control reads as a field.
    if matches!(
        ty.as_str(),
        "date" | "time" | "datetime-local" | "month" | "week"
    ) {
        let placeholder = match ty.as_str() {
            "date" => "mm/dd/yyyy",
            "time" => "--:-- --",
            "datetime-local" => "mm/dd/yyyy --:-- --",
            "month" => "mm/yyyy",
            "week" => "Week --, ----",
            _ => "",
        };
        let value = attr("value").unwrap_or("");
        return Some(if value.is_empty() {
            placeholder.to_string()
        } else {
            value.to_string()
        });
    }
    // File chooser: a "Choose File" button label followed by the chosen filename (or the
    // conventional "No file chosen"). The button chrome comes from the UA stylesheet border.
    if ty == "file" {
        return Some("Choose File  No file chosen".to_string());
    }
    None
}

/// Parse a numeric attribute (`min`/`max`/`value`), returning `None` when absent/unparseable.
pub(crate) fn num_attr(el: &dom::ElementData, name: &str) -> Option<f32> {
    el.attrs
        .get(name)
        .and_then(|v| v.trim().parse::<f32>().ok())
}

/// Resolve an `<input type=range>`'s thumb position as a fraction (0..=1) of the track:
/// `(value - min) / (max - min)`, with the HTML defaults (min 0, max 100, value = midpoint).
pub(crate) fn range_fraction(el: &dom::ElementData) -> f32 {
    let min = num_attr(el, "min").unwrap_or(0.0);
    let max = num_attr(el, "max").unwrap_or(100.0);
    let span = max - min;
    let value = num_attr(el, "value").unwrap_or(min + span / 2.0);
    if span.abs() < f32::EPSILON {
        0.0
    } else {
        ((value - min) / span).clamp(0.0, 1.0)
    }
}

/// Parse a CSS hex color (`#rgb` / `#rrggbb`, leading `#` optional). Used for the `<input
/// type=color>` swatch (whose `value` is always a 7-char hex string per spec). `None` on failure.
pub(crate) fn parse_hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.trim().trim_start_matches('#');
    let hex = |a: u8, b: u8| u8::from_str_radix(&format!("{}{}", a as char, b as char), 16).ok();
    let bytes = h.as_bytes();
    match bytes.len() {
        3 => {
            let d = |c: u8| u8::from_str_radix(&format!("{}{}", c as char, c as char), 16).ok();
            Some((d(bytes[0])?, d(bytes[1])?, d(bytes[2])?))
        }
        6 => Some((
            hex(bytes[0], bytes[1])?,
            hex(bytes[2], bytes[3])?,
            hex(bytes[4], bytes[5])?,
        )),
        _ => None,
    }
}

/// The `<progress>`/`<meter>` fill fraction (0..=1) = `value / max` (max defaults to 1). For a
/// `<progress>` with no `value`, returns `None` (indeterminate). `is_progress` selects the
/// indeterminate behavior (meter always has a value: defaults to 0).
pub(crate) fn bar_fraction(el: &dom::ElementData, is_progress: bool) -> Option<f32> {
    let max = num_attr(el, "max").unwrap_or(1.0).max(f32::EPSILON);
    match num_attr(el, "value") {
        Some(v) => Some((v / max).clamp(0.0, 1.0)),
        None if is_progress => None, // indeterminate progress bar
        None => Some(0.0),           // a meter with no value reads as empty
    }
}

/// Give a drawn-widget box an explicit content size: the element's CSS width/height if set, else
/// the supplied intrinsic default. Widgets are replaced-element-like, so block layout must not try
/// to stretch/shrink them past this; we set the content rect directly (like the image/caret path).
pub(crate) fn size_widget_box(
    bx: &mut LayoutBox,
    cs: &style::ComputedStyle,
    default_w: f32,
    default_h: f32,
) {
    bx.dimensions.content.width = cs.width.unwrap_or(default_w).max(1.0);
    bx.dimensions.content.height = cs.height.unwrap_or(default_h).max(1.0);
}

/// The label a `<select>` (id `select_id`) should display in its (single-line) dropdown control:
/// the text of its selected `<option>`. Walks all descendant `<option>` elements depth-first
/// (including those nested inside `<optgroup>`), and picks, in priority order:
///   1. the `<option>` carrying a `selected` attribute;
///   2. else, if the `<select>` has a `value` attribute, the `<option>` whose value
///      (its `value` attr, or — when it has no `value` attr — its collapsed text) equals it;
///   3. else the FIRST `<option>`.
/// Returns the chosen option's collapsed text, or `""` when the `<select>` has no options.
/// (A `<select multiple>` / `size>1` is a multi-row listbox in real browsers; for v1 we still
/// render the single selected/first label, which is acceptable.)
pub(crate) fn selected_option_text(doc: &dom::Document, select_id: dom::NodeId) -> String {
    // Collect descendant <option> ids depth-first.
    let mut options: Vec<dom::NodeId> = Vec::new();
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
    walk(doc, select_id, &mut options);
    if options.is_empty() {
        return String::new();
    }

    // The collapsed text content of an <option> (its descendant text nodes).
    let option_text = |opt: dom::NodeId| -> String {
        let mut s = String::new();
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
        gather(doc, opt, &mut s);
        collapse_whitespace(&s)
    };

    // 1. An <option selected>.
    for &opt in &options {
        if let dom::NodeData::Element(el) = &doc.get(opt).data {
            if el.attrs.contains_key("selected") {
                return option_text(opt);
            }
        }
    }

    // 2. The <option> whose value matches the <select>'s `value` attribute.
    if let dom::NodeData::Element(sel) = &doc.get(select_id).data {
        if let Some(want) = sel.attrs.get("value") {
            for &opt in &options {
                if let dom::NodeData::Element(el) = &doc.get(opt).data {
                    let val = match el.attrs.get("value") {
                        Some(v) => v.clone(),
                        None => option_text(opt),
                    };
                    if &val == want {
                        return option_text(opt);
                    }
                }
            }
        }
    }

    // 3. The first option.
    option_text(options[0])
}

/// True if `el` is a field that should show a text caret when focused: a `<textarea>` or a
/// text-like `<input>` (mirrors `input_display_text`'s text-like set; excludes button-like inputs).
pub(crate) fn is_caret_field(el: &dom::ElementData) -> bool {
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

/// Build the paint style for an element from its computed style.
pub(crate) fn paint_style_of(cs: &style::ComputedStyle) -> PaintStyle {
    PaintStyle {
        color: cs.color,
        font_size: cs.font_size,
        bold: cs.bold,
        italic: cs.italic,
        background_color: cs.background_color,
        border_color: cs.border_color,
        border_collapse: cs.border_collapse,
        is_table_cell: cs.display == style::Display::TableCell,
        underline: cs.underline,
        line_through: cs.line_through,
        overline: cs.overline,
        vertical_align: cs.vertical_align,
        white_space: cs.white_space,
        opacity: cs.opacity,
        visible: cs.visibility == style::Visibility::Visible,
        letter_spacing: cs.letter_spacing,
        line_height: cs.line_height,
        font_family: cs.font_family.as_deref().map(Box::from),
        // Only allocate the extras box when the element actually has a gradient/shadow/transform/
        // border-radius (all rare). border-radius lives here to keep the common PaintStyle small.
        extras: if cs.background_gradient.is_some()
            || !cs.box_shadows.is_empty()
            || cs.transform.is_some()
            || cs.border_radius != 0.0
            || cs.mask_image.is_some()
            || cs.background_image_url.is_some()
        {
            Some(Box::new(PaintExtras {
                background_gradient: cs.background_gradient.clone(),
                box_shadows: cs.box_shadows.clone(),
                transform: cs.transform,
                transform_origin: cs.transform_origin,
                border_radius: cs.border_radius,
                mask_image: cs.mask_image.clone(),
                background_image: cs.background_image_url.as_ref().map(|url| style::BgImage {
                    url: url.clone(),
                    size: cs.background_size,
                    repeat: cs.background_repeat,
                    position: cs.background_position,
                }),
            }))
        } else {
            None
        },
    }
}

/// Convert a `style::Edges` into a layout `Edges`.
pub(crate) fn edges_of(e: style::Edges) -> Edges {
    Edges {
        top: e.top,
        right: e.right,
        bottom: e.bottom,
        left: e.left,
    }
}

/// Immutable inputs threaded through the (mutually recursive) box-tree builder. Bundling them in
/// one reference keeps the recursive `build_box`/`build_children` stack frames small (deep DOM
/// nesting recurses here), and gives the caret/checkbox code access to the focused node id.
pub(crate) struct BuildCtx<'a> {
    pub(crate) styles: &'a HashMap<dom::NodeId, style::ComputedStyle>,
    pub(crate) intrinsic_sizes: &'a HashMap<dom::NodeId, (f32, f32)>,
    pub(crate) focused: Option<dom::NodeId>,
}

/// Build the child boxes for `parent_id`'s children, wrapping runs of inline children in
/// anonymous blocks when the parent also contains block children.
pub(crate) fn build_children(
    doc: &dom::Document,
    parent_id: dom::NodeId,
    bx: &BuildCtx,
) -> Vec<LayoutBox> {
    let styles = bx.styles;
    // First, produce a flat list of child boxes (each tagged block vs inline).
    let mut flat: Vec<LayoutBox> = Vec::new();
    for &child in &doc.get(parent_id).children {
        // Defensive: never index the arena with a stale/garbage child id (see prune_invalid).
        if child.0 >= doc.len() {
            continue;
        }
        build_box(doc, child, bx, &mut flat);
    }

    // Classify each box as block-level for flow purposes. A block-level Image counts as a block;
    // an inline-level (atomic) Image counts as inline content.
    let is_block_level = |b: &LayoutBox| match &b.content {
        BoxContent::Block => true,
        BoxContent::Image(_) => image_is_block(b, styles),
        _ => false,
    };

    // If there are no block-level children, no anonymous wrapping is needed.
    let has_block = flat.iter().any(&is_block_level);
    let has_inline = flat.iter().any(|b| {
        matches!(b.content, BoxContent::Inline | BoxContent::Text(_))
            || (matches!(b.content, BoxContent::Image(_)) && !image_is_block(b, styles))
    });
    if !(has_block && has_inline) {
        return flat;
    }

    // Mixed: wrap consecutive inline/text runs in anonymous block boxes.
    let mut out: Vec<LayoutBox> = Vec::new();
    let mut run: Vec<LayoutBox> = Vec::new();
    for b in flat {
        if is_block_level(&b) {
            if !run.is_empty() {
                out.push(make_anonymous(std::mem::take(&mut run)));
            }
            out.push(b);
        } else {
            run.push(b);
        }
    }
    if !run.is_empty() {
        out.push(make_anonymous(run));
    }
    out
}

/// Wrap inline children in an anonymous block box.
pub(crate) fn make_anonymous(children: Vec<LayoutBox>) -> LayoutBox {
    let mut anon = LayoutBox::new(BoxContent::Anonymous, PaintStyle::default(), None);
    anon.children = children;
    anon
}

/// Build the box for a replaced element (`<img>`) or a form control (`<input>`/`<textarea>`).
/// Returns `None` only for a zero-sized image (nothing to draw). Kept out of `build_box` (and
/// `#[inline(never)]`) so its locals don't enlarge the recursive box-builder stack frame.
#[inline(never)]
pub(crate) fn build_replaced_or_control(
    doc: &dom::Document,
    el: &dom::ElementData,
    id: dom::NodeId,
    cs: &style::ComputedStyle,
    intrinsic_sizes: &HashMap<dom::NodeId, (f32, f32)>,
    focused: Option<dom::NodeId>,
) -> Option<LayoutBox> {
    let out_of_flow = matches!(
        cs.position,
        style::Position::Absolute | style::Position::Fixed
    );
    let block_display = matches!(
        cs.display,
        style::Display::Block | style::Display::Flex | style::Display::Grid
    ) || (cs.display == style::Display::Inline && cs.display_block);
    let is_block = out_of_flow || block_display;

    // <img> / <canvas> / <svg>: a replaced box sized from CSS width/height and/or intrinsic dims.
    // <canvas> is a replaced element whose intrinsic size is its width/height attributes (default
    // 300x150); the engine rasterizes its display list into a bitmap and composites it like an img.
    // Inline <svg>'s intrinsic size is seeded into `intrinsic_sizes` by the engine (width/height
    // attrs, else its viewBox, else 300x150); the engine rasterizes the SVG subtree to a bitmap.
    if el.tag.eq_ignore_ascii_case("img")
        || el.tag.eq_ignore_ascii_case("canvas")
        || el.tag.eq_ignore_ascii_case("svg")
    {
        let is_canvas = el.tag.eq_ignore_ascii_case("canvas");
        let is_img = el.tag.eq_ignore_ascii_case("img");
        let intrinsic = if is_canvas {
            // Prefer the explicit width/height attributes; fall back to the spec default 300x150.
            let aw = el
                .attrs
                .get("width")
                .and_then(|v| v.trim().parse::<f32>().ok());
            let ah = el
                .attrs
                .get("height")
                .and_then(|v| v.trim().parse::<f32>().ok());
            Some((aw.unwrap_or(300.0).max(1.0), ah.unwrap_or(150.0).max(1.0)))
        } else {
            intrinsic_sizes.get(&id).copied()
        };
        // Presentational width/height HTML attributes on <img> set the used size (plain numbers →
        // CSS px). CSS still wins when present: only fill in a dimension the cascade left unset.
        let (mut css_w, mut css_h) = (cs.width, cs.height);
        if is_img {
            let aw = el
                .attrs
                .get("width")
                .and_then(|v| v.trim().parse::<f32>().ok());
            let ah = el
                .attrs
                .get("height")
                .and_then(|v| v.trim().parse::<f32>().ok());
            if css_w.is_none() {
                css_w = aw;
            }
            if css_h.is_none() {
                css_h = ah;
            }
        }
        let (cw, ch) = image_content_size(css_w, css_h, intrinsic);
        if cw <= 0.0 || ch <= 0.0 {
            // No drawable bitmap and no explicit size. For <img> with alt text, lay out a small
            // box containing the alt string so a broken image isn't a 0×0 nothing.
            if is_img {
                if let Some(alt) = el.attrs.get("alt") {
                    let alt = collapse_whitespace(alt);
                    if !alt.is_empty() {
                        let mut aps = paint_style_of(cs);
                        if aps.font_size <= 0.0 {
                            aps.font_size = 16.0;
                        }
                        let mut bx = LayoutBox::new(BoxContent::Block, aps.clone(), Some(id));
                        bx.dimensions.margin = edges_of(cs.margin);
                        bx.dimensions.padding = edges_of(cs.padding);
                        bx.dimensions.border = edges_of(cs.border);
                        bx.children
                            .push(LayoutBox::new(BoxContent::Text(alt), aps, Some(id)));
                        return Some(bx);
                    }
                }
            }
            return None; // nothing known to draw; skip producing a box
        }
        let mut bx = LayoutBox::new(BoxContent::Image(id), paint_style_of(cs), Some(id));
        bx.dimensions.margin = edges_of(cs.margin);
        bx.dimensions.padding = edges_of(cs.padding);
        bx.dimensions.border = edges_of(cs.border);
        bx.dimensions.content.width = cw;
        bx.dimensions.content.height = ch;
        return Some(bx);
    }

    let content = if is_block {
        BoxContent::Block
    } else {
        BoxContent::Inline
    };
    let ps = paint_style_of(cs);
    let mut bx = LayoutBox::new(content, ps.clone(), Some(id));
    bx.dimensions.margin = edges_of(cs.margin);
    bx.dimensions.padding = edges_of(cs.padding);
    bx.dimensions.border = edges_of(cs.border);

    // <progress> / <meter>: a horizontal bar widget (track + proportional fill). Sized to a
    // conventional 160×16 (honoring any explicit CSS width/height); the painter draws the bar.
    if el.tag.eq_ignore_ascii_case("progress") || el.tag.eq_ignore_ascii_case("meter") {
        let is_progress = el.tag.eq_ignore_ascii_case("progress");
        let kind = if is_progress {
            WidgetKind::Progress {
                fraction: bar_fraction(el, true),
            }
        } else {
            WidgetKind::Meter {
                fraction: bar_fraction(el, false).unwrap_or(0.0),
            }
        };
        size_widget_box(&mut bx, cs, 160.0, 16.0);
        bx.content = BoxContent::Widget(kind);
        return Some(bx);
    }

    let input_ty = el
        .attrs
        .get("type")
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let is_input = el.tag.eq_ignore_ascii_case("input");

    // Checkbox / radio: a small (~13px) drawn box/circle reflecting the checked state. Drawn by the
    // painter (the ☑/☐/●/○ code points aren't in the bundled font), keeping the existing toggle.
    if is_input && (input_ty == "checkbox" || input_ty == "radio") {
        let checked = el.attrs.contains_key("checked");
        let kind = if input_ty == "checkbox" {
            WidgetKind::Checkbox { checked }
        } else {
            WidgetKind::Radio { checked }
        };
        size_widget_box(&mut bx, cs, 13.0, 13.0);
        bx.content = BoxContent::Widget(kind);
        return Some(bx);
    }

    // <input type=range>: a horizontal slider (track + thumb) at the value's position.
    if is_input && input_ty == "range" {
        size_widget_box(&mut bx, cs, 129.0, 21.0);
        bx.content = BoxContent::Widget(WidgetKind::Range {
            fraction: range_fraction(el),
        });
        return Some(bx);
    }

    // <input type=color>: a small swatch filled with the chosen color (default #000000).
    if is_input && input_ty == "color" {
        let rgb = el
            .attrs
            .get("value")
            .and_then(|v| parse_hex_color(v))
            .unwrap_or((0, 0, 0));
        size_widget_box(&mut bx, cs, 44.0, 23.0);
        bx.content = BoxContent::Widget(WidgetKind::Color { rgb });
        return Some(bx);
    }

    // <select>: render as a single-line dropdown control showing the selected option's label
    // plus a trailing dropdown arrow. The <option>/<optgroup> children are NOT laid out (the
    // caller stops recursing for <select>), so only the chosen label shows.
    if el.tag.eq_ignore_ascii_case("select") {
        let label = selected_option_text(doc, id);
        let mut sps = ps;
        if sps.font_size <= 0.0 {
            sps.font_size = 13.0;
        }
        let text = format!("{label}  \u{25BE}"); // U+25BE ▾
        bx.children
            .push(LayoutBox::new(BoxContent::Text(text), sps, Some(id)));
        return Some(bx);
    }

    // Text-like control: render its value/placeholder (and, when focused, a caret bar).
    let textarea_default = if el.tag.eq_ignore_ascii_case("textarea") {
        textarea_text_content(doc, id)
    } else {
        String::new()
    };
    if let Some(label) = input_display_text(el, &textarea_default) {
        let caret = focused == Some(id) && is_caret_field(el);
        // The value/placeholder text. When focused on a caret field, the "label" includes the
        // placeholder only when there's no real value; browsers hide the placeholder while editing,
        // so suppress it and show just the caret. We can tell value from placeholder by checking
        // the raw `value` attribute.
        let has_value = el
            .attrs
            .get("value")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
            || el.tag.eq_ignore_ascii_case("textarea");
        let show_text = if caret && !has_value {
            String::new()
        } else {
            label
        };
        if !show_text.is_empty() {
            bx.children.push(LayoutBox::new(
                BoxContent::Text(show_text),
                ps.clone(),
                Some(id),
            ));
        }
        if caret {
            // A thin vertical bar ≈ the cap height of the control's text, in the foreground color.
            // It flows inline (atomically) so it sits right after the value text (or at the start
            // of an empty field). Vertically centered on the line via a top margin.
            let fs = if ps.font_size > 0.0 {
                ps.font_size
            } else {
                16.0
            };
            let cps = ps;
            let mut cbx = LayoutBox::new(BoxContent::Caret, cps.clone(), Some(id));
            let caret_h = (fs * 0.8).round().max(1.0); // ≈ cap height
            cbx.dimensions.content.width = 2.0;
            cbx.dimensions.content.height = caret_h;
            // Center the bar on the text line: the line advance is ~font line-height; split the
            // slack above/below. Use a top margin so the atomic placement drops the bar down.
            let line_h = cps.line_height.unwrap_or(fs * 1.2);
            let top = ((line_h - caret_h) / 2.0).max(0.0);
            cbx.dimensions.margin.top = top;
            cbx.dimensions.margin.bottom = (line_h - caret_h - top).max(0.0);
            bx.children.push(cbx);
        }
        return Some(bx);
    }

    // Any other input type (hidden/file/color/range/date…): a styled, empty box (matching the old
    // fall-through to generic element layout — inputs are void, so there are no children to add).
    Some(bx)
}

/// Build an anonymous generated-content box for a `::before`/`::after` pseudo-element from its
/// computed style `cs`. The box is inline by default (so it flows with the element's text) unless
/// the pseudo style says block/flex/grid. It holds a single `Text` child with the resolved content
/// string. The box itself carries NO DOM node (it is anonymous, with no backing element) — the
/// `originating` id is only used so the text run inherits a sensible style lookup if needed.
///
/// Returns `None` only for `display: none`. An empty content string still yields a box (it may
/// carry a visible background/border); the inner `Text` child is skipped when the string is empty.
pub(crate) fn build_pseudo_box(
    originating: dom::NodeId,
    cs: &style::ComputedStyle,
) -> Option<LayoutBox> {
    if cs.display_none {
        return None;
    }
    let content_str = cs.content.clone().unwrap_or_default();
    let block_display = matches!(
        cs.display,
        style::Display::Block | style::Display::Flex | style::Display::Grid
    ) || (cs.display == style::Display::Inline && cs.display_block);
    let content = if block_display {
        BoxContent::Block
    } else {
        BoxContent::Inline
    };
    let ps = paint_style_of(cs);
    // Anonymous: no node id (matches other anonymous boxes), so layout/paint never tries to read
    // a (nonexistent) style entry for it.
    let mut bx = LayoutBox::new(content, ps.clone(), None);
    bx.dimensions.margin = edges_of(cs.margin);
    bx.dimensions.padding = edges_of(cs.padding);
    bx.dimensions.border = edges_of(cs.border);
    if !content_str.is_empty() {
        // The text run carries the originating element's id so its paint style resolves the same
        // way ordinary text does if the box's own style isn't consulted directly.
        bx.children.push(LayoutBox::new(
            BoxContent::Text(content_str),
            ps,
            Some(originating),
        ));
    }
    Some(bx)
}

/// Build the box (or boxes) for a single DOM node, pushing into `out`. May push nothing
/// (hidden / non-rendered / empty text) or several (an inline element contributes its own
/// box; its rendered text/children become that box's children).
pub(crate) fn build_box(
    doc: &dom::Document,
    id: dom::NodeId,
    bx_ctx: &BuildCtx,
    out: &mut Vec<LayoutBox>,
) {
    let styles = bx_ctx.styles;
    let intrinsic_sizes = bx_ctx.intrinsic_sizes;
    let focused = bx_ctx.focused;
    let node = doc.get(id);
    match &node.data {
        dom::NodeData::Text(text) => {
            // Under `white-space: pre`/`pre-wrap` (inherited from the nearest element), spaces and
            // newlines are PRESERVED: emitted as `Text` runs split by `LineBreak`s (helper keeps
            // this recursive frame small). The runs carry `white_space` so inline layout doesn't
            // re-collapse them.
            let ws = nearest_element_white_space(doc, id, styles);
            if ws.preserves_spaces() {
                push_pre_text(doc, id, text, styles, out);
                return;
            }
            let collapsed = collapse_whitespace(text);
            if collapsed.is_empty() {
                return;
            }
            // Text nodes inherit paint info from the nearest element ancestor; the cascade
            // stores a style for elements only, so look up the parent element's style.
            let ps = nearest_element_style(doc, id, styles);
            // Apply text-transform (inherited from the nearest element) to the rendered string so
            // the transformed text is what gets measured + painted.
            let transform = nearest_element_text_transform(doc, id, styles);
            let transformed = apply_text_transform(&collapsed, transform);
            let tb = LayoutBox::new(BoxContent::Text(transformed), ps, Some(id));
            out.push(tb);
        }
        dom::NodeData::Element(el) => {
            if is_non_rendered_tag(&el.tag) {
                return;
            }
            let cs = match styles.get(&id) {
                Some(cs) => cs,
                None => return,
            };
            if cs.display_none {
                return;
            }
            // <br>: a forced line break. Emit a LineBreak box (inline-level, no glyphs); inline
            // layout ends the current line and starts a new one when it sees it.
            if el.tag.eq_ignore_ascii_case("br") {
                out.push(LayoutBox::new(
                    BoxContent::LineBreak,
                    paint_style_of(cs),
                    Some(id),
                ));
                return;
            }
            // Replaced elements (<img>) and form controls (<input>/<textarea>) build a dedicated
            // box (image / glyph / value text). Handled in a non-recursive helper so this frame —
            // which recurses for deep DOM nesting — stays small.
            if el.tag.eq_ignore_ascii_case("img")
                || el.tag.eq_ignore_ascii_case("canvas")
                || el.tag.eq_ignore_ascii_case("svg")
                || el.tag.eq_ignore_ascii_case("input")
                || el.tag.eq_ignore_ascii_case("textarea")
                || el.tag.eq_ignore_ascii_case("select")
                || el.tag.eq_ignore_ascii_case("progress")
                || el.tag.eq_ignore_ascii_case("meter")
            {
                if let Some(produced) =
                    build_replaced_or_control(doc, el, id, cs, intrinsic_sizes, focused)
                {
                    out.push(produced);
                }
                // For these tags we never fall through to generic element layout (img has no
                // rendered children; inputs/textareas render their value, not their DOM subtree;
                // a <select> renders only the selected option's label, not its <option> subtree).
                // A `None` from the helper (e.g. a zero-sized image, or `type=hidden`) drops the box.
                return;
            }
            // A box is block-level in its parent's flow if it generates a block-level box
            // (Block/Flex/Grid) or is out-of-flow (Absolute/Fixed are treated as block-level
            // so they aren't merged into inline runs). Inline / inline-block / inline-flex /
            // inline-grid are inline-level.
            let out_of_flow = matches!(
                cs.position,
                style::Position::Absolute | style::Position::Fixed
            );
            // Honor the legacy `display_block` flag too, so styles constructed the old way (only
            // `display_block: true`, `display` left at its Inline default) still lay out as blocks.
            let block_display = is_block_level_display(cs.display)
                || (cs.display == style::Display::Inline && cs.display_block);
            let is_block = out_of_flow || block_display;
            let content = if is_block {
                BoxContent::Block
            } else {
                BoxContent::Inline
            };
            // Build children FIRST (the deep recursion happens here) so this element's large
            // `LayoutBox` is not alive on the stack during descent — keeps the recursive frame small.
            let mut children: Vec<LayoutBox> = Vec::new();
            if let Some(before) = &cs.before {
                if let Some(b) = build_pseudo_box(id, before) {
                    children.push(b);
                }
            }
            // List-item marker: a leading bullet/number box positioned in the list's left padding.
            // Generated for `<li>` (decimal markers count `<li>` siblings) unless `list-style-type:
            // none`. In an `#[inline(never)]` helper so this recursive frame stays small.
            if el.tag.eq_ignore_ascii_case("li") {
                push_li_marker(doc, id, cs, &mut children);
            }
            children.extend(grow_stack(|| build_children(doc, id, bx_ctx)));
            if let Some(after) = &cs.after {
                if let Some(b) = build_pseudo_box(id, after) {
                    children.push(b);
                }
            }
            // Block-in-inline: an inline element that contains block-level children is blockified so
            // those blocks get a real box (CSS splits the inline around the blocks; we approximate by
            // making the inline parent a block container). Without this, e.g. a block inside a <span>
            // — or the block shadow content of an inline custom-element host — never lays out.
            let content = if matches!(content, BoxContent::Inline)
                && children
                    .iter()
                    .any(|c| matches!(c.content, BoxContent::Block))
            {
                BoxContent::Block
            } else {
                content
            };
            // Assemble this element's box after recursion unwinds.
            let mut bx = LayoutBox::new(content, paint_style_of(cs), Some(id));
            bx.dimensions.margin = edges_of(cs.margin);
            bx.dimensions.padding = edges_of(cs.padding);
            bx.dimensions.border = edges_of(cs.border);
            bx.children = children;
            out.push(bx);
        }
        _ => {
            // Document / Comment nodes contribute nothing themselves, but a Document child
            // (shouldn't normally appear mid-tree) would have its children walked elsewhere.
        }
    }
}

/// Emit the boxes for a `white-space: pre`/`pre-wrap` text node: spaces are preserved and each
/// source line becomes a `Text` run carrying `white_space` (so inline layout keeps it atomic), with
/// a `LineBreak` between consecutive lines (so multi-line `<pre>` content renders on multiple lines,
/// including blank lines). `#[inline(never)]` to keep the recursive box-builder frame small.
#[inline(never)]
pub(crate) fn push_pre_text(
    doc: &dom::Document,
    id: dom::NodeId,
    text: &str,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    out: &mut Vec<LayoutBox>,
) {
    let ps = nearest_element_style(doc, id, styles);
    let transform = nearest_element_text_transform(doc, id, styles);
    let mut lines = text.split('\n').peekable();
    while let Some(seg) = lines.next() {
        if !seg.is_empty() {
            let rendered = apply_text_transform(seg, transform);
            out.push(LayoutBox::new(
                BoxContent::Text(rendered),
                ps.clone(),
                Some(id),
            ));
        }
        if lines.peek().is_some() {
            out.push(LayoutBox::new(BoxContent::LineBreak, ps.clone(), Some(id)));
        }
    }
}

/// Generate an `<li>`'s marker box (bullet/number) and insert it as the first of `children`, unless
/// the list-style-type is `none`. `#[inline(never)]` to keep the recursive box-builder frame small.
#[inline(never)]
pub(crate) fn push_li_marker(
    doc: &dom::Document,
    id: dom::NodeId,
    cs: &style::ComputedStyle,
    children: &mut Vec<LayoutBox>,
) {
    if let Some(marker) = li_marker_text(doc, id, cs) {
        let mut mps = paint_style_of(cs);
        if mps.font_size <= 0.0 {
            mps.font_size = 16.0;
        }
        children.insert(
            0,
            LayoutBox::new(BoxContent::Marker(marker.into()), mps, Some(id)),
        );
    }
}

/// Compute the marker string for an `<li>` from its computed `list-style-type` (inherited from the
/// enclosing `ul`/`ol`). Bullet types render a glyph; `decimal` renders the 1-based ordinal of this
/// `<li>` among its `<li>` siblings followed by a dot. `None` for `list-style-type: none`.
pub(crate) fn li_marker_text(
    doc: &dom::Document,
    id: dom::NodeId,
    cs: &style::ComputedStyle,
) -> Option<String> {
    match cs.list_style_type {
        style::ListStyleType::None => None,
        style::ListStyleType::Disc => Some("\u{2022}".to_string()), // •
        style::ListStyleType::Circle => Some("\u{25E6}".to_string()), // ◦
        style::ListStyleType::Square => Some("\u{25AA}".to_string()), // ▪
        style::ListStyleType::Decimal => {
            let mut ordinal = 0usize;
            if let Some(parent) = doc.get(id).parent {
                for &sib in &doc.get(parent).children {
                    if sib.0 >= doc.len() {
                        continue;
                    }
                    if let dom::NodeData::Element(e) = &doc.get(sib).data {
                        if e.tag.eq_ignore_ascii_case("li") {
                            ordinal += 1;
                            if sib == id {
                                break;
                            }
                        }
                    }
                }
            }
            Some(format!("{}.", ordinal.max(1)))
        }
    }
}

/// Find the paint style for a text node by walking up to the nearest element ancestor that
/// has a computed style. Falls back to a default.
pub(crate) fn nearest_element_style(
    doc: &dom::Document,
    mut id: dom::NodeId,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> PaintStyle {
    // The text node itself has no style entry; climb parents.
    while let Some(parent) = doc.get(id).parent {
        if let Some(cs) = styles.get(&parent) {
            return paint_style_of(cs);
        }
        id = parent;
    }
    PaintStyle::default()
}

/// The `white-space` of the nearest element ancestor of node `id` (defaults to Normal). Resolved at
/// box-build time so the inline layout never needs the document (it reads the box tree only).
pub(crate) fn nearest_element_white_space(
    doc: &dom::Document,
    id: dom::NodeId,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> style::WhiteSpace {
    if let Some(cs) = styles.get(&id) {
        return cs.white_space;
    }
    let mut id = id;
    while let Some(parent) = doc.get(id).parent {
        if let Some(cs) = styles.get(&parent) {
            return cs.white_space;
        }
        id = parent;
    }
    style::WhiteSpace::Normal
}

/// Find the `text-transform` of the nearest element ancestor of a text node (defaults to None).
pub(crate) fn nearest_element_text_transform(
    doc: &dom::Document,
    mut id: dom::NodeId,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> style::TextTransform {
    while let Some(parent) = doc.get(id).parent {
        if let Some(cs) = styles.get(&parent) {
            return cs.text_transform;
        }
        id = parent;
    }
    style::TextTransform::None
}

/// Apply a CSS `text-transform` to a string. `Capitalize` upper-cases the first letter of each
/// whitespace-separated word.
pub(crate) fn apply_text_transform(s: &str, t: style::TextTransform) -> String {
    match t {
        style::TextTransform::None => s.to_string(),
        style::TextTransform::Uppercase => s.to_uppercase(),
        style::TextTransform::Lowercase => s.to_lowercase(),
        style::TextTransform::Capitalize => {
            let mut out = String::with_capacity(s.len());
            let mut at_word_start = true;
            for ch in s.chars() {
                if ch.is_whitespace() {
                    at_word_start = true;
                    out.push(ch);
                } else if at_word_start {
                    out.extend(ch.to_uppercase());
                    at_word_start = false;
                } else {
                    out.push(ch);
                }
            }
            out
        }
    }
}

/// Collapse runs of ASCII whitespace into single spaces and trim the ends.
pub(crate) fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;
    for ch in s.chars() {
        if ch.is_ascii_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(ch);
            last_was_space = false;
        }
    }
    // Trim only ASCII whitespace (the collapsible kind) — NOT Unicode whitespace such as `&nbsp;`
    // (U+00A0), which CSS preserves. Rust's `str::trim()` would strip nbsp, dropping a meaningful
    // glyph (e.g. the width contributed by a trailing `&nbsp;`).
    out.trim_matches(|c: char| c.is_ascii_whitespace())
        .to_string()
}
