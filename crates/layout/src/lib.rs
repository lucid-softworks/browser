//! Box-model layout. Turns the styled DOM into a tree of positioned boxes.
//!
//! This file defines the *public contract* (geometry types, the paint-facing [`LayoutBox`],
//! the [`TextMeasurer`] trait, and [`layout_document`]). The block/inline layout algorithm
//! that fills it in is implemented against these types. Consumers (the engine's painter)
//! depend only on the shapes here, never on how layout is computed.

use std::collections::HashMap;

/// An axis-aligned rectangle in CSS pixels (top-left origin, y grows downward).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// The four sides of a box (margin / border / padding thicknesses), in pixels.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Edges {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

/// The box-model geometry of a single box: its content rect plus the surrounding
/// padding / border / margin thicknesses.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Dimensions {
    pub content: Rect,
    pub padding: Edges,
    pub border: Edges,
    pub margin: Edges,
}

impl Dimensions {
    /// The padding box = content expanded by padding.
    pub fn padding_box(&self) -> Rect {
        expand(self.content, self.padding)
    }
    /// The border box = padding box expanded by border. This is what backgrounds fill.
    pub fn border_box(&self) -> Rect {
        expand(self.padding_box(), self.border)
    }
    /// The margin box = border box expanded by margin.
    pub fn margin_box(&self) -> Rect {
        expand(self.border_box(), self.margin)
    }
}

fn expand(r: Rect, e: Edges) -> Rect {
    Rect {
        x: r.x - e.left,
        y: r.y - e.top,
        width: r.width + e.left + e.right,
        height: r.height + e.top + e.bottom,
    }
}

/// Everything the painter needs to draw a box, lifted out of the computed style so the
/// painter never has to re-consult the style map.
#[derive(Debug, Clone, PartialEq)]
pub struct PaintStyle {
    pub color: (u8, u8, u8),
    pub font_size: f32,
    pub bold: bool,
    pub italic: bool,
    pub background_color: Option<(u8, u8, u8)>,
    pub border_color: (u8, u8, u8),
    /// Draw an underline under text runs (`text-decoration: underline`).
    pub underline: bool,
    /// Draw a strike-through line over text runs (`text-decoration: line-through`).
    pub line_through: bool,
    /// Draw an overline above text runs (`text-decoration: overline`).
    pub overline: bool,
    /// `vertical-align` (sub/super) for inline runs. Drives the painter's baseline shift.
    pub vertical_align: style::VerticalAlign,
    /// `white-space` mode (collapse vs preserve spaces/newlines). Read by inline layout to decide
    /// whether a text run's spaces are preserved and `\n`s are forced breaks.
    pub white_space: style::WhiteSpace,
    /// Per-box opacity (0.0..=1.0); the painter multiplies painted alpha by this (and threads it
    /// to the subtree). 1.0 = fully opaque.
    pub opacity: f32,
    /// Extra px advance added per character (`letter-spacing`). Painter uses it to space glyphs.
    pub letter_spacing: f32,
    /// Resolved `line-height` in px (`None` = use the font metric). Drives inline line advance.
    pub line_height: Option<f32>,
    /// Rarely-used paint features (gradients / shadows / transforms / border-radius), boxed so the
    /// common box stays small (one pointer) and allocates nothing — `None` = none of them set.
    pub extras: Option<Box<PaintExtras>>,
}

/// Gradient / box-shadow / transform paint data, kept behind an `Option<Box<…>>` on [`PaintStyle`]
/// so unstyled boxes (the overwhelming majority) carry only a null pointer and never allocate.
#[derive(Debug, Clone, PartialEq)]
pub struct PaintExtras {
    /// A `background-image` gradient (linear/radial), if any. Painted as the box's background.
    pub background_gradient: Option<style::Gradient>,
    /// `box-shadow` layers, painted back-to-front. Empty = none.
    pub box_shadows: Vec<style::BoxShadow>,
    /// Composed 2D affine `transform` `[a b c d e f]` (local space, pre-origin). `None` = identity.
    pub transform: Option<[f32; 6]>,
    /// `transform-origin` as fractions of the box's own size (x, y); default (0.5, 0.5).
    pub transform_origin: (f32, f32),
    /// Uniform corner radius (px) for the background/border (0 = square). Lives here (rather than on
    /// the hot `PaintStyle`) to keep the per-box paint style small for deeply nested layouts.
    pub border_radius: f32,
}

impl Default for PaintStyle {
    fn default() -> Self {
        PaintStyle {
            color: (0, 0, 0),
            font_size: 0.0,
            bold: false,
            italic: false,
            background_color: None,
            border_color: (0, 0, 0),
            underline: false,
            line_through: false,
            overline: false,
            vertical_align: style::VerticalAlign::Baseline,
            white_space: style::WhiteSpace::Normal,
            opacity: 1.0,
            letter_spacing: 0.0,
            line_height: None,
            extras: None,
        }
    }
}

impl PaintStyle {
    /// The uniform corner radius (px); 0 when no `border-radius` is set (it lives in [`PaintExtras`]
    /// to keep the common `PaintStyle` small).
    pub fn border_radius(&self) -> f32 {
        self.extras.as_deref().map(|e| e.border_radius).unwrap_or(0.0)
    }
}

/// What a box contains.
#[derive(Debug, Clone, PartialEq)]
pub enum BoxContent {
    /// A block-level box (stacks vertically, fills available width).
    Block,
    /// An inline-level box (flows horizontally into line boxes).
    Inline,
    /// An anonymous box created to hold inline content / line boxes.
    Anonymous,
    /// A run of laid-out text. The string is the text to paint. Normally whitespace-collapsed; under
    /// `white-space: pre`/`pre-wrap` (per the box's `style.white_space`) spaces are preserved and the
    /// run is treated atomically by line layout.
    Text(String),
    /// A list-item marker (the bullet / number string for an `<li>`). Positioned by layout in the
    /// list's left padding (to the left of the li content) and painted as text. `Box<str>` (16B,
    /// not 24B) so this extra variant doesn't widen `BoxContent` past its niche-packed 24 bytes.
    Marker(Box<str>),
    /// A forced line break (`<br>`, or a preserved `\n` under `white-space: pre`). Carries no
    /// glyphs; inline layout ends the current line box and starts a new one.
    LineBreak,
    /// A replaced image box for the given DOM node. Sized from CSS width/height and/or the
    /// node's intrinsic size; the painter blits the decoded pixels into its content rect.
    Image(dom::NodeId),
    /// The text-insertion caret of a focused text-like control: a thin vertical bar. The painter
    /// fills its content rect with the box's foreground (`style.color`); its width/height are set
    /// at build time (a ~2px-wide bar ≈ the font's cap height). It flows inline (atomically) so it
    /// sits immediately after the value text (or at the start of an empty field).
    Caret,
    /// A native-looking form widget the painter draws as shapes (no glyphs), sized at build time:
    /// a checkbox/radio box, a range slider, a color swatch, or a progress/meter bar. The
    /// element's attributes (`checked`/`value`/`min`/`max`) are resolved to the variant's fields at
    /// build time so the painter only draws. Flows atomically (like an image) so it sits inline.
    Widget(WidgetKind),
}

/// A form control the painter renders as primitive shapes (rects/circles/lines) rather than text.
/// Built from the element's attributes in `build_replaced_or_control`; the painter only draws.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WidgetKind {
    /// `<input type=checkbox>`: a small square; `checked` → a filled check mark.
    Checkbox { checked: bool },
    /// `<input type=radio>`: a small circle; `checked` → a filled inner dot.
    Radio { checked: bool },
    /// `<input type=range>`: a horizontal track with a thumb at `fraction` (0..=1) along it.
    Range { fraction: f32 },
    /// `<input type=color>`: a swatch filled with `rgb`, thin border.
    Color { rgb: (u8, u8, u8) },
    /// `<progress>`: a track with a fill = `fraction` (0..=1); `None` = indeterminate (full fill).
    Progress { fraction: Option<f32> },
    /// `<meter>`: a track with a greenish fill = `fraction` (0..=1).
    Meter { fraction: f32 },
}

/// A node in the layout tree: geometry + paint info + children.
#[derive(Debug, Clone)]
pub struct LayoutBox {
    pub dimensions: Dimensions,
    pub content: BoxContent,
    /// The DOM node this box came from, if any (anonymous/line boxes have none).
    pub node: Option<dom::NodeId>,
    pub style: PaintStyle,
    pub children: Vec<LayoutBox>,
}

impl LayoutBox {
    pub fn new(content: BoxContent, style: PaintStyle, node: Option<dom::NodeId>) -> Self {
        LayoutBox {
            dimensions: Dimensions::default(),
            content,
            node,
            style,
            children: Vec::new(),
        }
    }
}

/// How the layout engine measures text. Implemented by the engine over its font so layout
/// stays decoupled from font rasterization.
pub trait TextMeasurer {
    /// Advance width (px) of `text` rendered at `px` size (with optional faux-bold).
    fn text_width(&self, text: &str, px: f32, bold: bool) -> f32;
    /// The line height (px) for text rendered at `px` size.
    fn line_height(&self, px: f32) -> f32;
}

/// Lay out `doc` (with its computed `styles`) into a tree of positioned boxes that fits a
/// viewport `viewport_width` pixels wide. Height is driven by content. The returned root box
/// is positioned at (0, 0); the painter walks it.
pub fn layout_document(
    doc: &dom::Document,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    viewport_width: f32,
    viewport_height: f32,
    measurer: &dyn TextMeasurer,
    intrinsic_sizes: &HashMap<dom::NodeId, (f32, f32)>,
    focused: Option<dom::NodeId>,
) -> LayoutBox {
    // 1. Build the box tree from the DOM (skipping hidden / non-rendered subtrees), inserting
    //    anonymous blocks where block and inline siblings mix. Image boxes are sized from their
    //    intrinsic dimensions (and any CSS width/height) during layout. `focused` is the node id
    //    of the focused text field, which gets a `BoxContent::Caret` bar after its value text.
    // Snapshot every table cell's colspan/rowspan from the DOM so `layout_table` (which only sees
    // the box tree + styles) can honor them without a new threaded parameter.
    capture_table_spans(doc);
    let mut root = LayoutBox::new(BoxContent::Block, PaintStyle::default(), None);
    let bx_ctx = BuildCtx { styles, intrinsic_sizes, focused };
    root.children = build_children(doc, doc.root(), &bx_ctx);

    // 2. The root is the viewport block. Lay it out against a containing block that is the
    //    viewport: origin (0,0), width = viewport_width.
    let viewport = Rect { x: 0.0, y: 0.0, width: viewport_width, height: viewport_height };
    let containing = Rect { x: 0.0, y: 0.0, width: viewport_width, height: 0.0 };
    // The initial containing block (for absolutes with no positioned ancestor) is the viewport.
    let ctx = Ctx { positioned: viewport, viewport };
    layout_block(&mut root, containing, ctx, styles, measurer);
    root
}

/// Positioning context threaded through layout:
/// - `positioned`: the padding box of the nearest positioned ancestor (the containing block for
///   `position: absolute` descendants). Starts as the viewport (the initial containing block).
/// - `viewport`: the viewport rect (the containing block for `position: fixed`).
#[derive(Debug, Clone, Copy)]
struct Ctx {
    positioned: Rect,
    viewport: Rect,
}

/// The explicit content width set on a box's node (if any).
fn explicit_width(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> Option<f32> {
    boxx.node.and_then(|n| styles.get(&n)).and_then(|cs| cs.width)
}

/// The explicit content height set on a box's node (if any).
fn explicit_height(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> Option<f32> {
    boxx.node.and_then(|n| styles.get(&n)).and_then(|cs| cs.height)
}

/// The computed style for a box's node, if any.
fn style_of<'a>(
    boxx: &LayoutBox,
    styles: &'a HashMap<dom::NodeId, style::ComputedStyle>,
) -> Option<&'a style::ComputedStyle> {
    boxx.node.and_then(|n| styles.get(&n))
}

/// Clamp a used content `width` to the box's `[min-width, max-width]` (resolved against the
/// containing block content width `cb_width`). `max-width` applies first per CSS, then
/// `min-width` (so min wins on conflict). A box with no node leaves the width unchanged.
fn clamp_width(
    boxx: &LayoutBox,
    width: f32,
    cb_width: f32,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> f32 {
    let cs = match style_of(boxx, styles) {
        Some(cs) => cs,
        None => return width,
    };
    let mut w = width;
    if let Some(max) = cs.max_width {
        w = w.min(max.resolve(cb_width));
    }
    if let Some(min) = cs.min_width {
        w = w.max(min.resolve(cb_width));
    }
    w.max(0.0)
}

/// Clamp a used content `height` to the box's `[min-height, max-height]` (resolved against the
/// containing block height `cb_height`). Percentages of an indefinite container height resolve
/// against `cb_height` (which may be 0 → percentage min/max effectively unset).
fn clamp_height(
    boxx: &LayoutBox,
    height: f32,
    cb_height: f32,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> f32 {
    let cs = match style_of(boxx, styles) {
        Some(cs) => cs,
        None => return height,
    };
    let mut h = height;
    if let Some(max) = cs.max_height {
        h = h.min(max.resolve(cb_height));
    }
    if let Some(min) = cs.min_height {
        h = h.max(min.resolve(cb_height));
    }
    h.max(0.0)
}

/// The `display` mode of a box (defaults to Block for anonymous/root boxes).
///
/// Reconciles the legacy `display_block`/`display_none` flags with the richer `display` enum:
/// a style constructed the old way (only `display_block: true`) still lays out as a block, and
/// `display_none` always wins. This keeps externally-constructed `ComputedStyle`s working.
fn display_of(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> style::Display {
    match style_of(boxx, styles) {
        None => style::Display::Block, // anonymous / root
        Some(cs) => {
            if cs.display_none {
                style::Display::None
            } else if cs.display == style::Display::Inline && cs.display_block {
                // Legacy flag set without the enum being updated.
                style::Display::Block
            } else {
                cs.display
            }
        }
    }
}

/// Compute an image's content-box size (width, height) from any CSS `width`/`height` and its
/// intrinsic size. Rules:
///   * both CSS dimensions set → use them;
///   * one CSS dimension set + an intrinsic aspect ratio known → scale the other to preserve it;
///   * one CSS dimension set, no intrinsic → use it for that axis, 0 for the other (skipped);
///   * no CSS dimensions → use the intrinsic size, or (0,0) if unknown.
fn image_content_size(
    css_w: Option<f32>,
    css_h: Option<f32>,
    intrinsic: Option<(f32, f32)>,
) -> (f32, f32) {
    match (css_w, css_h) {
        (Some(w), Some(h)) => (w.max(0.0), h.max(0.0)),
        (Some(w), None) => {
            let h = match intrinsic {
                Some((iw, ih)) if iw > 0.0 => w * (ih / iw),
                _ => 0.0,
            };
            (w.max(0.0), h.max(0.0))
        }
        (None, Some(h)) => {
            let w = match intrinsic {
                Some((iw, ih)) if ih > 0.0 => h * (iw / ih),
                _ => 0.0,
            };
            (w.max(0.0), h.max(0.0))
        }
        (None, None) => match intrinsic {
            Some((iw, ih)) => (iw.max(0.0), ih.max(0.0)),
            None => (0.0, 0.0),
        },
    }
}

/// True if an Image box is block-level (computed display block/flex/grid, the legacy
/// `display_block` flag, or out-of-flow). Otherwise the image is atomic inline-level.
fn image_is_block(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> bool {
    match style_of(boxx, styles) {
        None => false,
        Some(cs) => {
            let block_display = matches!(
                cs.display,
                style::Display::Block | style::Display::Flex | style::Display::Grid
            ) || (cs.display == style::Display::Inline && cs.display_block);
            let out_of_flow =
                matches!(cs.position, style::Position::Absolute | style::Position::Fixed);
            block_display || out_of_flow
        }
    }
}

/// True for `display` values that produce a block-level box in their parent's flow (for box-tree
/// construction purposes): block/flex/grid, plus `table` (a table box is block-level) and the
/// table-internal display types (`table-row`, `table-cell`, row groups, caption, columns). The
/// table-internal boxes are kept as structural (Block-content) boxes so `layout_table` can walk
/// them; they are never wrapped in anonymous blocks because they only appear under a table.
fn is_block_level_display(d: style::Display) -> bool {
    matches!(
        d,
        style::Display::Block
            | style::Display::Flex
            | style::Display::Grid
            | style::Display::Table
            | style::Display::TableRow
            | style::Display::TableCell
            | style::Display::TableRowGroup
            | style::Display::TableHeaderGroup
            | style::Display::TableFooterGroup
            | style::Display::TableCaption
            | style::Display::TableColumn
            | style::Display::TableColumnGroup
    )
}

/// The `position` of a box (defaults to Static).
fn position_of(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> style::Position {
    style_of(boxx, styles).map(|cs| cs.position).unwrap_or(style::Position::Static)
}

/// True if a box is taken out of normal flow (absolutely or fixed positioned).
fn is_out_of_flow(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> bool {
    matches!(position_of(boxx, styles), style::Position::Absolute | style::Position::Fixed)
}

/// The text alignment of a box's node (defaults to Left).
fn text_align_of(
    node: Option<dom::NodeId>,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> TextAlignLocal {
    match node.and_then(|n| styles.get(&n)).map(|cs| cs.text_align) {
        Some(style::TextAlign::Center) => TextAlignLocal::Center,
        Some(style::TextAlign::Right) => TextAlignLocal::Right,
        _ => TextAlignLocal::Left,
    }
}

// ---------------------------------------------------------------------------------------------
// Box-tree construction
// ---------------------------------------------------------------------------------------------

/// Tags whose subtrees are never rendered (metadata / scripting).
fn is_non_rendered_tag(tag: &str) -> bool {
    matches!(
        tag.to_ascii_lowercase().as_str(),
        "script" | "style" | "head" | "title" | "noscript" | "template" | "meta" | "link"
    )
}

/// The text a form control (`<input>` / `<textarea>`) should render inside its box, or `None`
/// if `el` isn't such a control. Returns `Some(String)` (possibly empty → a styled-but-empty box):
///   * `<textarea>`: its live `value` attribute (or empty);
///   * text-like `<input>` (text/search/email/url/tel/password/number/no-type): its `value`
///     (with `type=password` masked to bullets), else its `placeholder`, else empty;
///   * `<input type=submit|button|reset>`: its `value` as the button label (defaulting to a
///     conventional label when absent);
///   * other input types (checkbox/radio/hidden/file/image/color/range/date…): `None` (no text).
fn input_display_text(el: &dom::ElementData) -> Option<String> {
    let attr = |name: &str| el.attrs.get(name).map(|s| s.as_str());
    if el.tag.eq_ignore_ascii_case("textarea") {
        return Some(attr("value").unwrap_or("").to_string());
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
    if matches!(ty.as_str(), "date" | "time" | "datetime-local" | "month" | "week") {
        let placeholder = match ty.as_str() {
            "date" => "mm/dd/yyyy",
            "time" => "--:-- --",
            "datetime-local" => "mm/dd/yyyy --:-- --",
            "month" => "mm/yyyy",
            "week" => "Week --, ----",
            _ => "",
        };
        let value = attr("value").unwrap_or("");
        return Some(if value.is_empty() { placeholder.to_string() } else { value.to_string() });
    }
    // File chooser: a "Choose File" button label followed by the chosen filename (or the
    // conventional "No file chosen"). The button chrome comes from the UA stylesheet border.
    if ty == "file" {
        return Some("Choose File  No file chosen".to_string());
    }
    None
}

/// Parse a numeric attribute (`min`/`max`/`value`), returning `None` when absent/unparseable.
fn num_attr(el: &dom::ElementData, name: &str) -> Option<f32> {
    el.attrs.get(name).and_then(|v| v.trim().parse::<f32>().ok())
}

/// Resolve an `<input type=range>`'s thumb position as a fraction (0..=1) of the track:
/// `(value - min) / (max - min)`, with the HTML defaults (min 0, max 100, value = midpoint).
fn range_fraction(el: &dom::ElementData) -> f32 {
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
fn parse_hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.trim().trim_start_matches('#');
    let hex = |a: u8, b: u8| u8::from_str_radix(&format!("{}{}", a as char, b as char), 16).ok();
    let bytes = h.as_bytes();
    match bytes.len() {
        3 => {
            let d = |c: u8| u8::from_str_radix(&format!("{}{}", c as char, c as char), 16).ok();
            Some((d(bytes[0])?, d(bytes[1])?, d(bytes[2])?))
        }
        6 => Some((hex(bytes[0], bytes[1])?, hex(bytes[2], bytes[3])?, hex(bytes[4], bytes[5])?)),
        _ => None,
    }
}

/// The `<progress>`/`<meter>` fill fraction (0..=1) = `value / max` (max defaults to 1). For a
/// `<progress>` with no `value`, returns `None` (indeterminate). `is_progress` selects the
/// indeterminate behavior (meter always has a value: defaults to 0).
fn bar_fraction(el: &dom::ElementData, is_progress: bool) -> Option<f32> {
    let max = num_attr(el, "max").unwrap_or(1.0).max(f32::EPSILON);
    match num_attr(el, "value") {
        Some(v) => Some((v / max).clamp(0.0, 1.0)),
        None if is_progress => None,    // indeterminate progress bar
        None => Some(0.0),              // a meter with no value reads as empty
    }
}

/// Give a drawn-widget box an explicit content size: the element's CSS width/height if set, else
/// the supplied intrinsic default. Widgets are replaced-element-like, so block layout must not try
/// to stretch/shrink them past this; we set the content rect directly (like the image/caret path).
fn size_widget_box(bx: &mut LayoutBox, cs: &style::ComputedStyle, default_w: f32, default_h: f32) {
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
fn selected_option_text(doc: &dom::Document, select_id: dom::NodeId) -> String {
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
fn is_caret_field(el: &dom::ElementData) -> bool {
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

/// Build the paint style for an element from its computed style.
fn paint_style_of(cs: &style::ComputedStyle) -> PaintStyle {
    PaintStyle {
        color: cs.color,
        font_size: cs.font_size,
        bold: cs.bold,
        italic: cs.italic,
        background_color: cs.background_color,
        border_color: cs.border_color,
        underline: cs.underline,
        line_through: cs.line_through,
        overline: cs.overline,
        vertical_align: cs.vertical_align,
        white_space: cs.white_space,
        opacity: cs.opacity,
        letter_spacing: cs.letter_spacing,
        line_height: cs.line_height,
        // Only allocate the extras box when the element actually has a gradient/shadow/transform/
        // border-radius (all rare). border-radius lives here to keep the common PaintStyle small.
        extras: if cs.background_gradient.is_some()
            || !cs.box_shadows.is_empty()
            || cs.transform.is_some()
            || cs.border_radius != 0.0
        {
            Some(Box::new(PaintExtras {
                background_gradient: cs.background_gradient.clone(),
                box_shadows: cs.box_shadows.clone(),
                transform: cs.transform,
                transform_origin: cs.transform_origin,
                border_radius: cs.border_radius,
            }))
        } else {
            None
        },
    }
}

/// Convert a `style::Edges` into a layout `Edges`.
fn edges_of(e: style::Edges) -> Edges {
    Edges { top: e.top, right: e.right, bottom: e.bottom, left: e.left }
}

/// Immutable inputs threaded through the (mutually recursive) box-tree builder. Bundling them in
/// one reference keeps the recursive `build_box`/`build_children` stack frames small (deep DOM
/// nesting recurses here), and gives the caret/checkbox code access to the focused node id.
struct BuildCtx<'a> {
    styles: &'a HashMap<dom::NodeId, style::ComputedStyle>,
    intrinsic_sizes: &'a HashMap<dom::NodeId, (f32, f32)>,
    focused: Option<dom::NodeId>,
}

/// Build the child boxes for `parent_id`'s children, wrapping runs of inline children in
/// anonymous blocks when the parent also contains block children.
fn build_children(
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
fn make_anonymous(children: Vec<LayoutBox>) -> LayoutBox {
    let mut anon = LayoutBox::new(BoxContent::Anonymous, PaintStyle::default(), None);
    anon.children = children;
    anon
}

/// Build the box for a replaced element (`<img>`) or a form control (`<input>`/`<textarea>`).
/// Returns `None` only for a zero-sized image (nothing to draw). Kept out of `build_box` (and
/// `#[inline(never)]`) so its locals don't enlarge the recursive box-builder stack frame.
#[inline(never)]
fn build_replaced_or_control(
    doc: &dom::Document,
    el: &dom::ElementData,
    id: dom::NodeId,
    cs: &style::ComputedStyle,
    intrinsic_sizes: &HashMap<dom::NodeId, (f32, f32)>,
    focused: Option<dom::NodeId>,
) -> Option<LayoutBox> {
    let out_of_flow = matches!(cs.position, style::Position::Absolute | style::Position::Fixed);
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
            let aw = el.attrs.get("width").and_then(|v| v.trim().parse::<f32>().ok());
            let ah = el.attrs.get("height").and_then(|v| v.trim().parse::<f32>().ok());
            Some((aw.unwrap_or(300.0).max(1.0), ah.unwrap_or(150.0).max(1.0)))
        } else {
            intrinsic_sizes.get(&id).copied()
        };
        // Presentational width/height HTML attributes on <img> set the used size (plain numbers →
        // CSS px). CSS still wins when present: only fill in a dimension the cascade left unset.
        let (mut css_w, mut css_h) = (cs.width, cs.height);
        if is_img {
            let aw = el.attrs.get("width").and_then(|v| v.trim().parse::<f32>().ok());
            let ah = el.attrs.get("height").and_then(|v| v.trim().parse::<f32>().ok());
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
                        bx.children.push(LayoutBox::new(BoxContent::Text(alt), aps, Some(id)));
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

    let content = if is_block { BoxContent::Block } else { BoxContent::Inline };
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
            WidgetKind::Progress { fraction: bar_fraction(el, true) }
        } else {
            WidgetKind::Meter { fraction: bar_fraction(el, false).unwrap_or(0.0) }
        };
        size_widget_box(&mut bx, cs, 160.0, 16.0);
        bx.content = BoxContent::Widget(kind);
        return Some(bx);
    }

    let input_ty =
        el.attrs.get("type").map(|s| s.trim().to_ascii_lowercase()).unwrap_or_default();
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
        bx.content = BoxContent::Widget(WidgetKind::Range { fraction: range_fraction(el) });
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
        bx.children.push(LayoutBox::new(BoxContent::Text(text), sps, Some(id)));
        return Some(bx);
    }

    // Text-like control: render its value/placeholder (and, when focused, a caret bar).
    if let Some(label) = input_display_text(el) {
        let caret = focused == Some(id) && is_caret_field(el);
        // The value/placeholder text. When focused on a caret field, the "label" includes the
        // placeholder only when there's no real value; browsers hide the placeholder while editing,
        // so suppress it and show just the caret. We can tell value from placeholder by checking
        // the raw `value` attribute.
        let has_value = el.attrs.get("value").map(|v| !v.is_empty()).unwrap_or(false)
            || el.tag.eq_ignore_ascii_case("textarea");
        let show_text = if caret && !has_value { String::new() } else { label };
        if !show_text.is_empty() {
            bx.children.push(LayoutBox::new(BoxContent::Text(show_text), ps.clone(), Some(id)));
        }
        if caret {
            // A thin vertical bar ≈ the cap height of the control's text, in the foreground color.
            // It flows inline (atomically) so it sits right after the value text (or at the start
            // of an empty field). Vertically centered on the line via a top margin.
            let fs = if ps.font_size > 0.0 { ps.font_size } else { 16.0 };
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
fn build_pseudo_box(originating: dom::NodeId, cs: &style::ComputedStyle) -> Option<LayoutBox> {
    if cs.display_none {
        return None;
    }
    let content_str = cs.content.clone().unwrap_or_default();
    let block_display = matches!(
        cs.display,
        style::Display::Block | style::Display::Flex | style::Display::Grid
    ) || (cs.display == style::Display::Inline && cs.display_block);
    let content = if block_display { BoxContent::Block } else { BoxContent::Inline };
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
        bx.children.push(LayoutBox::new(BoxContent::Text(content_str), ps, Some(originating)));
    }
    Some(bx)
}

/// Build the box (or boxes) for a single DOM node, pushing into `out`. May push nothing
/// (hidden / non-rendered / empty text) or several (an inline element contributes its own
/// box; its rendered text/children become that box's children).
fn build_box(
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
                out.push(LayoutBox::new(BoxContent::LineBreak, paint_style_of(cs), Some(id)));
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
            let out_of_flow = matches!(cs.position, style::Position::Absolute | style::Position::Fixed);
            // Honor the legacy `display_block` flag too, so styles constructed the old way (only
            // `display_block: true`, `display` left at its Inline default) still lay out as blocks.
            let block_display = is_block_level_display(cs.display)
                || (cs.display == style::Display::Inline && cs.display_block);
            let is_block = out_of_flow || block_display;
            let content = if is_block { BoxContent::Block } else { BoxContent::Inline };
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
            children.extend(build_children(doc, id, bx_ctx));
            if let Some(after) = &cs.after {
                if let Some(b) = build_pseudo_box(id, after) {
                    children.push(b);
                }
            }
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
fn push_pre_text(
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
            out.push(LayoutBox::new(BoxContent::Text(rendered), ps.clone(), Some(id)));
        }
        if lines.peek().is_some() {
            out.push(LayoutBox::new(BoxContent::LineBreak, ps.clone(), Some(id)));
        }
    }
}

/// Generate an `<li>`'s marker box (bullet/number) and insert it as the first of `children`, unless
/// the list-style-type is `none`. `#[inline(never)]` to keep the recursive box-builder frame small.
#[inline(never)]
fn push_li_marker(
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
        children.insert(0, LayoutBox::new(BoxContent::Marker(marker.into()), mps, Some(id)));
    }
}

/// Compute the marker string for an `<li>` from its computed `list-style-type` (inherited from the
/// enclosing `ul`/`ol`). Bullet types render a glyph; `decimal` renders the 1-based ordinal of this
/// `<li>` among its `<li>` siblings followed by a dot. `None` for `list-style-type: none`.
fn li_marker_text(
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
fn nearest_element_style(
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
fn nearest_element_white_space(
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
fn nearest_element_text_transform(
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
fn apply_text_transform(s: &str, t: style::TextTransform) -> String {
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
fn collapse_whitespace(s: &str) -> String {
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
    out.trim().to_string()
}

// ---------------------------------------------------------------------------------------------
// Block layout
// ---------------------------------------------------------------------------------------------

/// Remove a leading list-item `Marker` child from `boxx` (if present), boxed. `#[inline(never)]`
/// so its locals don't enlarge the recursive `layout_block` stack frame.
#[inline(never)]
fn take_marker(boxx: &mut LayoutBox) -> Option<Box<LayoutBox>> {
    if matches!(boxx.children.first().map(|c| &c.content), Some(BoxContent::Marker(_))) {
        Some(Box::new(boxx.children.remove(0)))
    } else {
        None
    }
}

/// Position a previously-extracted list-item marker `mb` to the LEFT of the box's content origin
/// (`x`, `y`) — in the list's left padding — and re-insert it as the first child for painting.
/// `#[inline(never)]` to keep `layout_block`'s frame small. The `mb` is taken boxed (despite
/// clippy's `boxed_local`) deliberately: it's the boxed `Option<Box<LayoutBox>>` held in the
/// deeply-recursive `layout_block` frame, so keeping it boxed avoids inflating that frame.
#[inline(never)]
#[allow(clippy::boxed_local)]
fn place_marker(boxx: &mut LayoutBox, mut mb: Box<LayoutBox>, x: f32, y: f32, measurer: &dyn TextMeasurer) {
    if let BoxContent::Marker(text) = &mb.content {
        let fs = if mb.style.font_size > 0.0 { mb.style.font_size } else { 16.0 };
        let mw = measurer.text_width(text, fs, mb.style.bold);
        let gap = (fs * 0.5).max(4.0);
        let lh = mb.style.line_height.unwrap_or_else(|| measurer.line_height(fs));
        mb.dimensions.content = Rect { x: (x - mw - gap).max(0.0), y, width: mw, height: lh };
    }
    boxx.children.insert(0, *mb);
}

/// Lay out a block box (or anonymous/root block) given its containing block's content rect.
/// Fills `boxx.dimensions.content` (position + width + height) and recurses. Dispatches to the
/// flex / grid algorithm when this box establishes such a formatting context.
fn layout_block(
    boxx: &mut LayoutBox,
    containing: Rect,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) {
    // A drawn form widget is replaced-like: its content size was fixed at build time. Place it like
    // an image (origin inside the containing block) and don't run block/inline formatting (which
    // would zero its pre-set height).
    if matches!(boxx.content, BoxContent::Widget(_) | BoxContent::Image(_)) {
        layout_image_box(boxx, containing);
        return;
    }

    let margin = boxx.dimensions.margin;
    let border = boxx.dimensions.border;
    let padding = boxx.dimensions.padding;

    // Explicit sizing comes from the box's DOM node's computed style.
    let explicit_w = explicit_width(boxx, styles);
    let explicit_h = explicit_height(boxx, styles);

    // Content width: containing content width minus this box's horizontal margin+border+padding,
    // unless an explicit width is set.
    let horizontal = margin.left + margin.right + border.left + border.right + padding.left
        + padding.right;
    let content_width = match explicit_w {
        Some(w) => w,
        None => (containing.width - horizontal).max(0.0),
    };
    // Clamp the used width to min-width / max-width (resolved against the containing block).
    let content_width = clamp_width(boxx, content_width, containing.width, styles);

    // Position: content origin sits inside the containing block, offset by left edges.
    let x = containing.x + margin.left + border.left + padding.left;
    let y = containing.y + margin.top + border.top + padding.top;

    boxx.dimensions.content = Rect { x, y, width: content_width, height: 0.0 };

    // List-item marker: pull a leading `Marker` child out of normal flow so it doesn't flow into the
    // content as a word. Positioned (in `place_marker`) once content height is known. Boxed +
    // `#[inline(never)]` helpers so the recursive `layout_block` frame stays small.
    let marker: Option<Box<LayoutBox>> = take_marker(boxx);

    // If this box is positioned (relative/absolute/fixed/sticky), it becomes the containing
    // block for its absolutely-positioned descendants. Update the context's `positioned` rect to
    // this box's padding box for children.
    let child_ctx = if !matches!(position_of(boxx, styles), style::Position::Static) {
        Ctx { positioned: boxx.dimensions.padding_box(), viewport: ctx.viewport }
    } else {
        ctx
    };

    // Dispatch on the display mode of this box.
    let display = display_of(boxx, styles);
    let content_height = match display {
        style::Display::Flex | style::Display::InlineFlex => {
            layout_flex(boxx, child_ctx, styles, measurer)
        }
        style::Display::Grid | style::Display::InlineGrid => {
            layout_grid(boxx, child_ctx, styles, measurer)
        }
        style::Display::Table => layout_table(boxx, child_ctx, styles, measurer),
        _ => {
            // Block / inline-block / (anonymous, root): normal block-or-inline formatting.
            let any_block = boxx
                .children
                .iter()
                .any(|c| matches!(c.content, BoxContent::Block | BoxContent::Anonymous)
                    || (matches!(c.content, BoxContent::Image(_) | BoxContent::Widget(_))
                        && image_is_block(c, styles)));
            if any_block {
                layout_block_children(boxx, child_ctx, styles, measurer)
            } else if !boxx.children.is_empty() {
                let align = text_align_of(boxx.node, styles);
                layout_inline_children(boxx, align, child_ctx, styles, measurer)
            } else {
                0.0
            }
        }
    };

    let final_height = explicit_h.unwrap_or(content_height);
    // Clamp the used height to min-height / max-height (% against the containing block height).
    let final_height = clamp_height(boxx, final_height, containing.height, styles);
    boxx.dimensions.content.height = final_height;

    // Position the list-item marker (if any) in the left padding, aligned with the first line.
    if let Some(mb) = marker {
        place_marker(boxx, mb, x, y, measurer);
    }

    // Resolve any out-of-flow children now that this box's geometry (and thus the containing
    // block for absolutes) is known. `child_ctx.positioned` was captured before this box's height
    // was resolved, so recompute the padding box here — otherwise `bottom`/`right` anchoring would
    // see a zero-height/zero-width containing block.
    let resolve_ctx = if !matches!(position_of(boxx, styles), style::Position::Static) {
        Ctx { positioned: boxx.dimensions.padding_box(), viewport: child_ctx.viewport }
    } else {
        child_ctx
    };
    resolve_out_of_flow(boxx, resolve_ctx, styles, measurer);

    // Apply a `position: relative` offset (after normal flow, without affecting siblings).
    apply_relative_offset(boxx, styles);
}

/// Lay out a block's block-level children top-to-bottom. Returns the total content height
/// (sum of child margin-box heights). No margin collapsing (kept simple). Out-of-flow children
/// are skipped here (they take no space) and resolved later.
fn layout_block_children(
    boxx: &mut LayoutBox,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) -> f32 {
    let content = boxx.dimensions.content;
    let parent_align = text_align_of(boxx.node, styles);
    let mut cursor_y = content.y;
    for child in &mut boxx.children {
        if is_out_of_flow(child, styles) {
            continue; // resolved separately; takes no space in flow
        }
        // Each child's containing block is this box's content rect, but positioned so the
        // child stacks below previous siblings. We thread the running y via the containing
        // rect's y, and the child adds its own top margin/border/padding inside layout_block.
        let containing = Rect { x: content.x, y: cursor_y, width: content.width, height: 0.0 };
        match &child.content {
            BoxContent::Block => layout_block(child, containing, ctx, styles, measurer),
            BoxContent::Image(_) | BoxContent::Widget(_) => layout_image_box(child, containing),
            BoxContent::Anonymous => {
                // Anonymous blocks inherit the establishing block's text-align.
                layout_anonymous(child, containing, parent_align, ctx, styles, measurer)
            }
            _ => {
                layout_anonymous(child, containing, parent_align, ctx, styles, measurer);
            }
        }
        cursor_y += child.dimensions.margin_box().height;
    }
    cursor_y - content.y
}

/// Position a replaced (image) box within `containing`. The content size was pre-computed at
/// box-tree build time (from CSS width/height and/or the intrinsic size); here we only place the
/// content origin inside the containing block, offset by the box's own margin/border/padding.
fn layout_image_box(boxx: &mut LayoutBox, containing: Rect) {
    let m = boxx.dimensions.margin;
    let b = boxx.dimensions.border;
    let p = boxx.dimensions.padding;
    let x = containing.x + m.left + b.left + p.left;
    let y = containing.y + m.top + b.top + p.top;
    boxx.dimensions.content.x = x;
    boxx.dimensions.content.y = y;
    // width/height already set at build time; leave them.
}

/// Resolve out-of-flow (absolute / fixed) children of `boxx`: size and position them against
/// their containing block, then lay out their own children.
fn resolve_out_of_flow(
    boxx: &mut LayoutBox,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) {
    for child in &mut boxx.children {
        match position_of(child, styles) {
            style::Position::Absolute => {
                layout_out_of_flow(child, ctx.positioned, ctx, styles, measurer)
            }
            style::Position::Fixed => {
                layout_out_of_flow(child, ctx.viewport, ctx, styles, measurer)
            }
            _ => {}
        }
    }
}

/// Lay out an out-of-flow box against `cb` (its containing block rect = padding box of the
/// nearest positioned ancestor, or the viewport). Insets resolve the position; size comes from
/// explicit width/height or content.
fn layout_out_of_flow(
    boxx: &mut LayoutBox,
    cb: Rect,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) {
    let cs = match style_of(boxx, styles) {
        Some(cs) => cs.clone(),
        None => return,
    };
    let margin = boxx.dimensions.margin;
    let border = boxx.dimensions.border;
    let padding = boxx.dimensions.padding;
    let horizontal = margin.left + margin.right + border.left + border.right + padding.left
        + padding.right;

    // Content width:
    //   * explicit `width` wins;
    //   * `left` AND `right` both set with no width => stretch to fill between them;
    //   * otherwise shrink-to-fit to the box's intrinsic (max-content) width, exactly like the
    //     inline-block path. `intrinsic_width` returns the border-box width (content + padding +
    //     border), so we strip the box's own horizontal padding/border back off to get content.
    let content_width = if let Some(w) = cs.width {
        w
    } else if let (Some(l), Some(r)) = (cs.left, cs.right) {
        (cb.width - l - r - horizontal).max(0.0)
    } else {
        let intrinsic = intrinsic_width(boxx, styles, measurer);
        let own_edges = padding.left + padding.right + border.left + border.right;
        // Never wider than the containing block's available content width.
        let avail = (cb.width - horizontal).max(0.0);
        (intrinsic - own_edges).max(0.0).min(avail)
    };
    // Clamp to min/max-width against the containing block.
    let content_width = clamp_width(boxx, content_width, cb.width, styles);

    // Tentative content origin: relative to the containing block's top-left, offset by insets.
    // The insets address the box's *margin* box edge; we then add the box's own left/top edges.
    let border_left_x = if let Some(l) = cs.left {
        cb.x + l
    } else if let Some(r) = cs.right {
        cb.x + cb.width - r - (content_width + horizontal) + margin.left
    } else {
        cb.x
    };
    let border_top_y = if let Some(t) = cs.top {
        cb.y + t
    } else if let Some(b) = cs.bottom {
        cb.y + cb.height - b // adjusted after height is known below
    } else {
        cb.y
    };

    let x = border_left_x + margin.left + border.left + padding.left;
    let y = border_top_y + margin.top + border.top + padding.top;
    boxx.dimensions.content = Rect { x, y, width: content_width, height: 0.0 };

    // This box is itself positioned, so it's the containing block for its abs descendants.
    let child_ctx = Ctx { positioned: boxx.dimensions.padding_box(), viewport: ctx.viewport };

    let display = display_of(boxx, styles);
    let content_height = match display {
        style::Display::Flex | style::Display::InlineFlex => {
            layout_flex(boxx, child_ctx, styles, measurer)
        }
        style::Display::Grid | style::Display::InlineGrid => {
            layout_grid(boxx, child_ctx, styles, measurer)
        }
        _ => {
            let any_block = boxx
                .children
                .iter()
                .any(|c| matches!(c.content, BoxContent::Block | BoxContent::Anonymous)
                    || (matches!(c.content, BoxContent::Image(_) | BoxContent::Widget(_))
                        && image_is_block(c, styles)));
            if any_block {
                layout_block_children(boxx, child_ctx, styles, measurer)
            } else if !boxx.children.is_empty() {
                let align = text_align_of(boxx.node, styles);
                layout_inline_children(boxx, align, child_ctx, styles, measurer)
            } else {
                0.0
            }
        }
    };
    let final_height = cs.height.unwrap_or(content_height);
    let final_height = clamp_height(boxx, final_height, cb.height, styles);
    boxx.dimensions.content.height = final_height;

    // If positioned by `bottom` (no `top`), re-anchor now that height is known.
    if cs.top.is_none() {
        if let Some(b) = cs.bottom {
            let new_border_top = cb.y + cb.height - b - (final_height + margin.top + margin.bottom
                + border.top + border.bottom + padding.top + padding.bottom);
            let new_y = new_border_top + margin.top + border.top + padding.top;
            shift_subtree(boxx, 0.0, new_y - boxx.dimensions.content.y);
        }
    }
    // Similarly for `right` with no `left` already handled above for x.

    // Recompute the containing block for nested absolutes now that this box's height (and any
    // `bottom` re-anchor shift) is final.
    let resolve_ctx = Ctx { positioned: boxx.dimensions.padding_box(), viewport: ctx.viewport };
    resolve_out_of_flow(boxx, resolve_ctx, styles, measurer);
}

/// Apply a `position: relative` offset to `boxx` and its whole subtree by the resolved
/// (left/right, top/bottom) insets, without affecting siblings.
fn apply_relative_offset(boxx: &mut LayoutBox, styles: &HashMap<dom::NodeId, style::ComputedStyle>) {
    let cs = match style_of(boxx, styles) {
        Some(cs) => cs,
        None => return,
    };
    if cs.position != style::Position::Relative {
        return;
    }
    let dx = if let Some(l) = cs.left {
        l
    } else if let Some(r) = cs.right {
        -r
    } else {
        0.0
    };
    let dy = if let Some(t) = cs.top {
        t
    } else if let Some(b) = cs.bottom {
        -b
    } else {
        0.0
    };
    if dx != 0.0 || dy != 0.0 {
        shift_subtree(boxx, dx, dy);
    }
}

/// Translate a box and all its descendants by (dx, dy).
fn shift_subtree(boxx: &mut LayoutBox, dx: f32, dy: f32) {
    boxx.dimensions.content.x += dx;
    boxx.dimensions.content.y += dy;
    for c in &mut boxx.children {
        shift_subtree(c, dx, dy);
    }
}

/// An anonymous block: same geometry rules as a block but with zero margins/border/padding and
/// inline-only children.
fn layout_anonymous(
    boxx: &mut LayoutBox,
    containing: Rect,
    align: TextAlignLocal,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) {
    boxx.dimensions.margin = Edges::default();
    boxx.dimensions.border = Edges::default();
    boxx.dimensions.padding = Edges::default();
    boxx.dimensions.content = Rect {
        x: containing.x,
        y: containing.y,
        width: containing.width,
        height: 0.0,
    };
    let h = layout_inline_children(boxx, align, ctx, styles, measurer);
    boxx.dimensions.content.height = h;
}

// ---------------------------------------------------------------------------------------------
// Intrinsic sizing
// ---------------------------------------------------------------------------------------------

/// Estimate the intrinsic content width of a box: explicit width if set, else the widest line
/// of text it would produce laid out unconstrained (max-content), plus its descendants' needs.
/// Used by inline-block (to size atomically) and flex (content base size).
fn intrinsic_width(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) -> f32 {
    if let Some(w) = explicit_width(boxx, styles) {
        let p = boxx.dimensions.padding;
        let b = boxx.dimensions.border;
        return w + p.left + p.right + b.left + b.right;
    }
    // Sum of own horizontal padding/border, plus the max child requirement.
    let p = boxx.dimensions.padding;
    let b = boxx.dimensions.border;
    let edges = p.left + p.right + b.left + b.right;

    // Gather all words in the subtree; the intrinsic (max-content) inline width is the sum of
    // word widths on a single unwrapped line for the longest text run. We approximate with the
    // widest single contiguous run of text.
    let mut max_inline = 0.0f32;
    let mut words: Vec<InlineWord> = Vec::new();
    collect_inline_words(&boxx.children, &mut words);
    if !words.is_empty() {
        let mut line_w = 0.0f32;
        for (i, w) in words.iter().enumerate() {
            let ww = run_width(measurer, &w.text, w.style.font_size, w.style.bold, w.style.letter_spacing);
            let sp = if i == 0 {
                0.0
            } else {
                measurer.text_width(" ", w.style.font_size, w.style.bold)
            };
            line_w += ww + sp;
        }
        max_inline = line_w;
    }

    // Reserve room for a focused-field caret bar (an inline atomic) so a shrink-to-fit control
    // doesn't clip the caret that sits right after its value text.
    for c in &boxx.children {
        if matches!(c.content, BoxContent::Caret) {
            max_inline += c.dimensions.margin_box().width;
        }
    }

    // Block children: the box is at least as wide as its widest block child.
    let mut max_block = 0.0f32;
    for c in &boxx.children {
        if matches!(c.content, BoxContent::Block | BoxContent::Anonymous) {
            max_block = max_block.max(intrinsic_width(c, styles, measurer));
        }
    }

    edges + max_inline.max(max_block)
}

// ---------------------------------------------------------------------------------------------
// Flexbox layout
// ---------------------------------------------------------------------------------------------

/// Lay out the flex items of `boxx` (a flex container whose content rect is already positioned
/// and width-sized). Returns the container's content height. Supports row/column (+ reverse),
/// wrap, gap, flex-grow/shrink/basis, justify-content, align-items/align-self.
fn layout_flex(
    boxx: &mut LayoutBox,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) -> f32 {
    let cs = style_of(boxx, styles).cloned().unwrap_or_default();
    let content = boxx.dimensions.content;
    let is_row = matches!(
        cs.flex_direction,
        style::FlexDirection::Row | style::FlexDirection::RowReverse
    );
    let reverse = matches!(
        cs.flex_direction,
        style::FlexDirection::RowReverse | style::FlexDirection::ColumnReverse
    );
    let main_gap = if is_row { cs.column_gap } else { cs.row_gap };
    let cross_gap = if is_row { cs.row_gap } else { cs.column_gap };

    // The main-axis available size. For row this is content width; for column we use explicit
    // height if set, else a large value (single line) — content drives the height.
    let main_avail = if is_row {
        content.width
    } else {
        explicit_height(boxx, styles).unwrap_or(f32::INFINITY)
    };
    let cross_container = if is_row {
        explicit_height(boxx, styles) // None → derived from lines
    } else {
        Some(content.width)
    };

    // Collect in-flow item indices (skip out-of-flow), ordered by `order` then source order.
    let mut items: Vec<usize> = boxx
        .children
        .iter()
        .enumerate()
        .filter(|(_, c)| !is_out_of_flow(c, styles))
        .map(|(i, _)| i)
        .collect();
    items.sort_by_key(|&i| order_of(&boxx.children[i], styles));

    // Compute each item's base main size + cross size + flex factors.
    struct Item {
        idx: usize,
        hyp_main: f32, // base main size after clamping (no min/max here → == base)
        grow: f32,
        shrink: f32,
        cross: f32,
        // box-edge sums along each axis (margin+border+padding)
        main_edges: f32,
        cross_edges: f32,
        align: style::AlignSelf,
    }
    let mut metas: Vec<Item> = Vec::new();
    for &i in &items {
        let child = &boxx.children[i];
        let ccs = style_of(child, styles).cloned().unwrap_or_default();
        let m = child.dimensions.margin;
        let b = child.dimensions.border;
        let p = child.dimensions.padding;
        let (main_edges, cross_edges) = if is_row {
            (
                m.left + m.right + b.left + b.right + p.left + p.right,
                m.top + m.bottom + b.top + b.bottom + p.top + p.bottom,
            )
        } else {
            (
                m.top + m.bottom + b.top + b.bottom + p.top + p.bottom,
                m.left + m.right + b.left + b.right + p.left + p.right,
            )
        };
        // Base main size (content-box): flex-basis, else explicit main size, else intrinsic.
        let base_content = if let Some(fb) = ccs.flex_basis {
            fb
        } else if is_row {
            ccs.width.unwrap_or_else(|| {
                (intrinsic_width(child, styles, measurer)
                    - (p.left + p.right + b.left + b.right))
                    .max(0.0)
            })
        } else {
            ccs.height.unwrap_or_else(|| intrinsic_cross_height(child, styles, measurer))
        };
        let base_main = base_content + main_edges;
        // Cross base size (content-box) for the item.
        let cross_content = if is_row {
            ccs.height.unwrap_or_else(|| intrinsic_cross_height(child, styles, measurer))
        } else {
            ccs.width.unwrap_or_else(|| {
                (intrinsic_width(child, styles, measurer)
                    - (p.left + p.right + b.left + b.right))
                    .max(0.0)
            })
        };
        metas.push(Item {
            idx: i,
            hyp_main: base_main,
            grow: ccs.flex_grow,
            shrink: ccs.flex_shrink,
            cross: cross_content + cross_edges,
            main_edges,
            cross_edges,
            align: ccs.align_self,
        });
    }

    // Break into flex lines (wrap if enabled and overflow).
    let wrap = !matches!(cs.flex_wrap, style::FlexWrap::NoWrap);
    let mut lines: Vec<Vec<usize>> = Vec::new(); // indices into `metas`
    if wrap && main_avail.is_finite() {
        let mut line: Vec<usize> = Vec::new();
        let mut used = 0.0f32;
        for (mi, m) in metas.iter().enumerate() {
            let add = if line.is_empty() { m.hyp_main } else { main_gap + m.hyp_main };
            if !line.is_empty() && used + add > main_avail {
                lines.push(std::mem::take(&mut line));
                used = m.hyp_main;
                line.push(mi);
            } else {
                used += add;
                line.push(mi);
            }
        }
        if !line.is_empty() {
            lines.push(line);
        }
    } else {
        lines.push((0..metas.len()).collect());
    }

    // Resolve each line: distribute free space, position along main & cross axes.
    let mut cross_cursor = if is_row { content.y } else { content.x };
    let mut line_cross_sizes: Vec<f32> = Vec::new();
    // First pass to know total cross used (for container sizing); we position as we go.
    let main_start = if is_row { content.x } else { content.y };

    // Determine final main container size for positioning.
    let main_box = if main_avail.is_finite() {
        main_avail
    } else {
        // single line, shrink-to-fit: sum of bases + gaps
        let sum: f32 = metas.iter().map(|m| m.hyp_main).sum::<f32>()
            + main_gap * (metas.len().saturating_sub(1) as f32);
        sum
    };

    for line in &lines {
        // Total base main size on this line.
        let n = line.len();
        let total_base: f32 = line.iter().map(|&mi| metas[mi].hyp_main).sum();
        let total_gap = main_gap * (n.saturating_sub(1) as f32);
        let free = main_box - total_base - total_gap;

        // Distribute free space: grow if positive, shrink if negative.
        let mut sizes: Vec<f32> = line.iter().map(|&mi| metas[mi].hyp_main).collect();
        if free > 0.0 {
            let total_grow: f32 = line.iter().map(|&mi| metas[mi].grow).sum();
            if total_grow > 0.0 {
                for (k, &mi) in line.iter().enumerate() {
                    sizes[k] += free * (metas[mi].grow / total_grow);
                }
            }
        } else if free < 0.0 {
            let total_shrink: f32 = line.iter().map(|&mi| metas[mi].shrink).sum();
            if total_shrink > 0.0 {
                for (k, &mi) in line.iter().enumerate() {
                    sizes[k] += free * (metas[mi].shrink / total_shrink);
                    if sizes[k] < 0.0 {
                        sizes[k] = 0.0;
                    }
                }
            }
        }

        let used_main: f32 = sizes.iter().sum::<f32>() + total_gap;
        let leftover = (main_box - used_main).max(0.0);

        // justify-content: leading offset + spacing between items.
        let (mut pos, between_extra) = match cs.justify_content {
            style::JustifyContent::FlexStart => (0.0, 0.0),
            style::JustifyContent::FlexEnd => (leftover, 0.0),
            style::JustifyContent::Center => (leftover / 2.0, 0.0),
            style::JustifyContent::SpaceBetween => {
                if n > 1 {
                    (0.0, leftover / (n - 1) as f32)
                } else {
                    (0.0, 0.0)
                }
            }
            style::JustifyContent::SpaceAround => {
                let each = if n > 0 { leftover / n as f32 } else { 0.0 };
                (each / 2.0, each)
            }
            style::JustifyContent::SpaceEvenly => {
                let each = leftover / (n + 1) as f32;
                (each, each)
            }
        };

        // Order items along main axis (reverse handled by iterating in reverse).
        let order_iter: Vec<usize> = if reverse {
            line.iter().rev().cloned().collect()
        } else {
            line.to_vec()
        };
        // sizes are indexed by position in `line`; build a lookup.
        let size_of = |mi: usize| -> f32 {
            let k = line.iter().position(|&x| x == mi).unwrap();
            sizes[k]
        };

        // ---- Pass A: lay out each item's contents at its constrained main size so we learn the
        // height (row) / extent each item actually occupies. The intrinsic estimate in `metas`
        // is only a single line of text; wrapped content can be taller, and a nested
        // flex/grid/block child can be much taller. We use the real laid-out height to size the
        // line cross extent (row) and so the next item / line clears it. Without this, a flex
        // item that reported one line but wrapped to three would let the following sibling sit on
        // top of it (the observed vertical overlap).
        let mut actual_cross: Vec<f32> = vec![0.0; metas.len()]; // per-meta, item margin-box cross
        let mut actual_laid: Vec<f32> = vec![0.0; metas.len()]; // per-meta, content height laid out
        for &mi in line {
            let item_main = size_of(mi);
            let meta = &metas[mi];
            let child = &mut boxx.children[meta.idx];
            // Tentatively size the content box so contents lay out at the right main extent.
            let content_main = (item_main - meta.main_edges).max(0.0);
            if is_row {
                child.dimensions.content.width = content_main;
                child.dimensions.content.height = (meta.cross - meta.cross_edges).max(0.0);
            } else {
                child.dimensions.content.height = content_main;
                child.dimensions.content.width = (meta.cross - meta.cross_edges).max(0.0);
            }
            let laid = layout_flex_item_contents(child, ctx, styles, measurer);
            actual_laid[mi] = laid;
            // The item's cross-axis margin-box extent: explicit cross size if set, else the
            // greater of the single-line estimate and what the contents actually needed.
            let has_explicit_cross = if is_row {
                explicit_height(child, styles).is_some()
            } else {
                explicit_width(child, styles).is_some()
            };
            let cross_extent = if is_row {
                if has_explicit_cross {
                    meta.cross
                } else {
                    (laid + meta.cross_edges).max(meta.cross)
                }
            } else {
                // column: cross is width — content layout doesn't change width, keep estimate.
                meta.cross
            };
            actual_cross[mi] = cross_extent;
        }

        // Cross size of this line = max actual item cross size, but if the container has an
        // explicit cross size and this is a single line, the line fills the container's cross
        // extent (so alignment / stretch are measured against the container box).
        let content_cross: f32 = line.iter().map(|&mi| actual_cross[mi]).fold(0.0, f32::max);
        let line_cross: f32 = match cross_container {
            Some(c) if lines.len() == 1 => c.max(content_cross),
            _ => content_cross,
        };

        // ---- Pass B: position each item now that the line cross size is known. For column flex
        // the per-item main extent grows to the laid-out height too, so items stack without
        // overlap; track the running main `pos` from the actual sizes.
        let mut col_pos = pos; // running main position for column (uses actual main extents)
        for &mi in &order_iter {
            let item_main = size_of(mi);
            let meta = &metas[mi];
            // Cross placement within the line.
            let align = match meta.align {
                style::AlignSelf::Auto => align_items_to_self(cs.align_items),
                other => other,
            };
            let this_cross = actual_cross[mi];
            let item_cross_outer = match align {
                style::AlignSelf::Stretch => line_cross,
                _ => this_cross,
            };
            let cross_off = match align {
                style::AlignSelf::FlexStart | style::AlignSelf::Stretch
                | style::AlignSelf::Baseline => 0.0,
                style::AlignSelf::FlexEnd => line_cross - this_cross,
                style::AlignSelf::Center => (line_cross - this_cross) / 2.0,
                style::AlignSelf::Auto => 0.0,
            };
            let cross_off = if matches!(align, style::AlignSelf::Stretch) { 0.0 } else { cross_off };

            // For column flex the main extent is the laid-out height (so the next item clears it).
            let main_extent = if is_row {
                item_main
            } else {
                let has_explicit_main = explicit_height(&boxx.children[meta.idx], styles).is_some();
                if has_explicit_main {
                    item_main
                } else {
                    let laid_main = actual_laid[mi] + meta.main_edges;
                    laid_main.max(item_main)
                }
            };

            // Compute the child's margin-box origin in (main, cross) then map to (x, y).
            let cur_main = if is_row { pos } else { col_pos };
            let main_origin = main_start + cur_main;
            let cross_origin = cross_cursor + cross_off;

            // Place the child. content size along main = item_main - main_edges; along cross =
            // item_cross_outer - cross_edges.
            let child = &mut boxx.children[meta.idx];
            let m = child.dimensions.margin;
            let b = child.dimensions.border;
            let p = child.dimensions.padding;
            let content_main = (item_main - meta.main_edges).max(0.0);
            let content_cross = (item_cross_outer - meta.cross_edges).max(0.0);

            let (cx, cy, cw, ch) = if is_row {
                (
                    main_origin + m.left + b.left + p.left,
                    cross_origin + m.top + b.top + p.top,
                    content_main,
                    content_cross,
                )
            } else {
                (
                    cross_origin + m.left + b.left + p.left,
                    main_origin + m.top + b.top + p.top,
                    content_cross,
                    (main_extent - meta.main_edges).max(0.0),
                )
            };
            child.dimensions.content = Rect { x: cx, y: cy, width: cw, height: ch };

            // Re-lay out contents at the final position so descendant boxes are correctly placed.
            layout_flex_item_contents(child, ctx, styles, measurer);

            if is_row {
                pos += item_main + main_gap + between_extra;
            } else {
                col_pos += main_extent + main_gap + between_extra;
            }
        }
        if !is_row {
            pos = col_pos;
        }
        let _ = pos;

        line_cross_sizes.push(line_cross);
        cross_cursor += line_cross + cross_gap;
    }

    // Container cross size = explicit, else sum of line cross sizes + gaps.
    let total_cross: f32 = line_cross_sizes.iter().sum::<f32>()
        + cross_gap * (line_cross_sizes.len().saturating_sub(1) as f32);
    if is_row {
        explicit_height(boxx, styles).unwrap_or(total_cross)
    } else {
        // column: height is the main size used.
        let used = line_cross_sizes; // not used here
        let _ = used;
        // main extent = bottom-most item; recompute from positions:
        let mut max_bottom = content.y;
        for c in &boxx.children {
            if !is_out_of_flow(c, styles) {
                max_bottom = max_bottom.max(c.dimensions.margin_box().y + c.dimensions.margin_box().height);
            }
        }
        explicit_height(boxx, styles).unwrap_or((max_bottom - content.y).max(0.0))
    }
}

/// The `order` of a flex item.
fn order_of(boxx: &LayoutBox, styles: &HashMap<dom::NodeId, style::ComputedStyle>) -> i32 {
    style_of(boxx, styles).map(|cs| cs.order).unwrap_or(0)
}

/// Map a container's `align-items` to the equivalent per-item `align-self`.
fn align_items_to_self(a: style::AlignItems) -> style::AlignSelf {
    match a {
        style::AlignItems::Stretch => style::AlignSelf::Stretch,
        style::AlignItems::FlexStart => style::AlignSelf::FlexStart,
        style::AlignItems::FlexEnd => style::AlignSelf::FlexEnd,
        style::AlignItems::Center => style::AlignSelf::Center,
        style::AlignItems::Baseline => style::AlignSelf::Baseline,
    }
}

/// An intrinsic cross-axis height estimate for a flex item (single line of text or explicit).
fn intrinsic_cross_height(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) -> f32 {
    if let Some(h) = explicit_height(boxx, styles) {
        return h;
    }
    // One line of text at the box's font size, or 0.
    let fs = boxx.style.font_size;
    let has_text = has_any_text(boxx);
    if has_text {
        measurer.line_height(if fs > 0.0 { fs } else { 16.0 })
    } else {
        0.0
    }
}

fn has_any_text(boxx: &LayoutBox) -> bool {
    if matches!(&boxx.content, BoxContent::Text(_)) {
        return true;
    }
    boxx.children.iter().any(has_any_text)
}

/// Lay out a flex item's own contents now that its content rect is fixed. The item itself acts
/// like a block container for its children. Returns the height the item's content actually
/// occupied once laid out at its constrained width (the intrinsic estimate used to size the
/// flex line is only an approximation; text can wrap to more lines than estimated). Callers use
/// this to grow the item / container so following items and siblings don't overlap.
fn layout_flex_item_contents(
    boxx: &mut LayoutBox,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) -> f32 {
    let child_ctx = if !matches!(position_of(boxx, styles), style::Position::Static) {
        Ctx { positioned: boxx.dimensions.padding_box(), viewport: ctx.viewport }
    } else {
        ctx
    };
    let display = display_of(boxx, styles);
    let laid_out = match display {
        style::Display::Flex | style::Display::InlineFlex => {
            layout_flex(boxx, child_ctx, styles, measurer)
        }
        style::Display::Grid | style::Display::InlineGrid => {
            layout_grid(boxx, child_ctx, styles, measurer)
        }
        _ => {
            let any_block = boxx
                .children
                .iter()
                .any(|c| matches!(c.content, BoxContent::Block | BoxContent::Anonymous)
                    || (matches!(c.content, BoxContent::Image(_) | BoxContent::Widget(_))
                        && image_is_block(c, styles)));
            if any_block {
                layout_block_children(boxx, child_ctx, styles, measurer)
            } else if !boxx.children.is_empty() {
                let align = text_align_of(boxx.node, styles);
                layout_inline_children(boxx, align, child_ctx, styles, measurer)
            } else {
                0.0
            }
        }
    };
    resolve_out_of_flow(boxx, child_ctx, styles, measurer);
    laid_out
}

// ---------------------------------------------------------------------------------------------
// Table layout (table formatting context)
// ---------------------------------------------------------------------------------------------

/// One table cell, lifted out of the table subtree for grid placement. `boxx` is the cell's own
/// `LayoutBox` (a `display: table-cell` box, with its content children); `col`/`row` are its
/// 0-based grid position; `colspan`/`rowspan` how many columns/rows it covers.
struct TableCell {
    boxx: LayoutBox,
    col: usize,
    row: usize,
    colspan: usize,
    rowspan: usize,
}

// Per-layout snapshot of every table cell's `colspan`/`rowspan` (keyed by the cell's DOM node id).
// Populated by [`layout_document`] from the DOM (where the attributes live) and read by
// [`layout_table`], which only has the styles map and the box tree — not the document. A
// thread-local keeps the spans out of the hot `LayoutBox`/`BoxContent` types (which a prior agent
// warned must stay small for deep-nesting stack safety) without threading a new parameter through
// every layout function.
thread_local! {
    static TABLE_SPANS: std::cell::RefCell<HashMap<dom::NodeId, (usize, usize)>> =
        std::cell::RefCell::new(HashMap::new());
}

/// Scan the whole document for `<td>`/`<th>` cells (or any element with a `colspan`/`rowspan`
/// attribute) and record their `(colspan, rowspan)` into the thread-local span snapshot. Values are
/// clamped to `[1, 1000]` so a hostile `colspan=100000000` can't blow up the column model.
fn capture_table_spans(doc: &dom::Document) {
    fn parse_span(el: &dom::ElementData, name: &str) -> usize {
        el.attrs
            .get(name)
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, 1000)
    }
    TABLE_SPANS.with(|t| {
        let mut map = t.borrow_mut();
        map.clear();
        for id in (0..doc.len()).map(dom::NodeId) {
            if let dom::NodeData::Element(el) = &doc.get(id).data {
                let cs = parse_span(el, "colspan");
                let rs = parse_span(el, "rowspan");
                if cs != 1 || rs != 1 {
                    map.insert(id, (cs, rs));
                }
            }
        }
    });
}

/// The `colspan` (`name == "colspan"`) or `rowspan` of a cell box, read from the thread-local span
/// snapshot (default 1 when the cell has no such attribute).
fn table_span(cell: &LayoutBox, name: &str) -> usize {
    let node = match cell.node {
        Some(n) => n,
        None => return 1,
    };
    TABLE_SPANS.with(|t| {
        t.borrow().get(&node).map(|(c, r)| if name == "colspan" { *c } else { *r }).unwrap_or(1)
    })
}

/// Gather the table's structure: descend through row-group wrappers (`thead`/`tbody`/`tfoot`,
/// recognized by `display`) and direct `<tr>` rows, in the spec's visual order (header groups
/// first, then body groups + direct rows in document order, then footer groups). Each returned
/// element is a list of cell `LayoutBox`es (the `display: table-cell` children of one `<tr>`).
/// Caption boxes are pulled out separately by the caller.
fn collect_table_rows(
    table: &mut LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> Vec<Vec<LayoutBox>> {
    // Split the table's direct children into header groups / body content / footer groups.
    let mut headers: Vec<Vec<LayoutBox>> = Vec::new();
    let mut bodies: Vec<Vec<LayoutBox>> = Vec::new();
    let mut footers: Vec<Vec<LayoutBox>> = Vec::new();

    // Drain the table's children so we can move cells out by value.
    let children = std::mem::take(&mut table.children);
    for child in children {
        let d = style_of(&child, styles).map(|cs| cs.display);
        match d {
            Some(style::Display::TableRow) => {
                bodies.push(extract_cells(child, styles));
            }
            Some(style::Display::TableHeaderGroup) => {
                collect_group_rows(child, styles, &mut headers);
            }
            Some(style::Display::TableFooterGroup) => {
                collect_group_rows(child, styles, &mut footers);
            }
            Some(style::Display::TableRowGroup) => {
                collect_group_rows(child, styles, &mut bodies);
            }
            // Caption / column(-group) / stray content: ignored here (captions handled separately,
            // columns don't produce rows). Anonymous or text boxes directly under a table are
            // dropped (they have no place in the row/cell grid).
            _ => {}
        }
    }

    let mut rows = headers;
    rows.append(&mut bodies);
    rows.append(&mut footers);
    rows
}

/// Append the `<tr>` rows found inside a row-group box (`thead`/`tbody`/`tfoot`) to `out`.
fn collect_group_rows(
    group: LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    out: &mut Vec<Vec<LayoutBox>>,
) {
    for row in group.children {
        if style_of(&row, styles).map(|cs| cs.display) == Some(style::Display::TableRow) {
            out.push(extract_cells(row, styles));
        }
    }
}

/// Extract the cell boxes (`display: table-cell`) that are children of one `<tr>` box.
fn extract_cells(
    row: LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> Vec<LayoutBox> {
    row.children
        .into_iter()
        .filter(|c| style_of(c, styles).map(|cs| cs.display) == Some(style::Display::TableCell))
        .collect()
}

/// The min-content width (px) of a cell: the widest single unbreakable word in its content
/// (so a column never gets narrower than its longest word), plus the cell's own horizontal
/// padding/border. Used as the lower bound for auto column sizing.
fn cell_min_content_width(
    boxx: &LayoutBox,
    measurer: &dyn TextMeasurer,
) -> f32 {
    let mut words: Vec<InlineWord> = Vec::new();
    collect_inline_words(&boxx.children, &mut words);
    let mut max_word = 0.0f32;
    for w in &words {
        for token in w.text.split_whitespace() {
            let ww = run_width(measurer, token, w.style.font_size, w.style.bold, w.style.letter_spacing);
            max_word = max_word.max(ww);
        }
    }
    let p = boxx.dimensions.padding;
    let b = boxx.dimensions.border;
    max_word + p.left + p.right + b.left + b.right
}

/// Lay out a `display: table` box as a grid of cells. The box's content rect (x/y/width) is already
/// positioned by `layout_block`; this fills in cell geometry and returns the table's content height.
///
/// Algorithm (auto table layout, simplified but column-aligned):
///   1. Collect rows (descending thead→tbody→tfoot groups + direct `<tr>`s) and their cells,
///      honoring `colspan`/`rowspan` via an occupancy grid so spanned slots are skipped.
///   2. Column count = max over rows of sum(colspan). Column widths = max over the column's cells of
///      their preferred (max-content) width, floored by the min-content width; columns are then
///      grown to fill an explicit table width and shrunk (proportionally, but never below
///      min-content) to fit the available width.
///   3. Row heights = max laid-out cell height in the row (a rowspan cell contributes to the last
///      row it covers). Cells are laid out as block containers at (column x, row y).
///   4. A `<caption>` is laid out full-width above the rows.
fn layout_table(
    boxx: &mut LayoutBox,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) -> f32 {
    // We need the document only for colspan/rowspan attributes; thread it via a thread-local-free
    // approach: those attrs were already validated into the style cascade? No — read from the DOM.
    // `layout_table` has no `doc`, so colspan/rowspan are read from a side channel set up by the
    // caller. Instead, we pull them from the cell box's node via the global doc passed through the
    // ctx-free path: we stored spans on build. To keep this self-contained, read them here from the
    // styles-independent attribute snapshot captured at build time (see `TABLE_SPANS`).
    let content = boxx.dimensions.content;
    let table_cs = style_of(boxx, styles).cloned().unwrap_or_default();

    // --- 1. Pull out captions (laid out above the grid) and collect rows of cells. ---
    // Captions are direct table children with display: table-caption.
    let mut captions: Vec<LayoutBox> = Vec::new();
    {
        let mut kept: Vec<LayoutBox> = Vec::new();
        for child in std::mem::take(&mut boxx.children) {
            if style_of(&child, styles).map(|cs| cs.display) == Some(style::Display::TableCaption) {
                captions.push(child);
            } else {
                kept.push(child);
            }
        }
        boxx.children = kept;
    }
    let row_cells = collect_table_rows(boxx, styles);

    // --- 2. Assign cells to grid positions honoring colspan/rowspan via an occupancy grid. ---
    let mut cells: Vec<TableCell> = Vec::new();
    // occupied[(row, col)] -> covered by a spanning cell.
    let mut occupied: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    let mut col_count = 0usize;
    for (r, cells_in_row) in row_cells.into_iter().enumerate() {
        let mut c = 0usize;
        for cell in cells_in_row {
            // Skip columns already covered by a rowspan from an earlier row.
            while occupied.contains(&(r, c)) {
                c += 1;
            }
            let colspan = table_span(&cell, "colspan");
            let rowspan = table_span(&cell, "rowspan");
            for dr in 0..rowspan {
                for dc in 0..colspan {
                    occupied.insert((r + dr, c + dc));
                }
            }
            col_count = col_count.max(c + colspan);
            cells.push(TableCell { boxx: cell, col: c, row: r, colspan, rowspan });
            c += colspan;
        }
    }
    let num_rows = cells.iter().map(|c| c.row + c.rowspan).max().unwrap_or(0);
    if col_count == 0 || cells.is_empty() {
        // Empty table: lay out any captions and report their height.
        let h = layout_table_captions(&mut captions, content, ctx, styles, measurer);
        boxx.children = captions;
        return h;
    }

    // --- 3. Column widths (auto layout). ---
    // Per-column min-content and preferred (max-content) widths from single-column cells.
    let mut col_min = vec![0.0f32; col_count];
    let mut col_pref = vec![0.0f32; col_count];
    for cell in &cells {
        if cell.colspan == 1 {
            let c = cell.col;
            col_min[c] = col_min[c].max(cell_min_content_width(&cell.boxx, measurer));
            col_pref[c] = col_pref[c].max(intrinsic_width(&cell.boxx, styles, measurer));
        }
    }
    // Spanning cells: ensure the spanned columns together are wide enough for the cell's content.
    for cell in &cells {
        if cell.colspan > 1 {
            let start = cell.col;
            let end = (start + cell.colspan).min(col_count);
            let span_min: f32 = col_min[start..end].iter().sum();
            let span_pref: f32 = col_pref[start..end].iter().sum();
            let need_min = cell_min_content_width(&cell.boxx, measurer);
            let need_pref = intrinsic_width(&cell.boxx, styles, measurer);
            let n = (end - start) as f32;
            if need_min > span_min && n > 0.0 {
                let add = (need_min - span_min) / n;
                for w in &mut col_min[start..end] {
                    *w += add;
                }
            }
            if need_pref > span_pref && n > 0.0 {
                let add = (need_pref - span_pref) / n;
                for w in &mut col_pref[start..end] {
                    *w += add;
                }
            }
        }
    }
    // Ensure preferred >= min per column.
    for c in 0..col_count {
        col_pref[c] = col_pref[c].max(col_min[c]);
    }

    let sum_pref: f32 = col_pref.iter().sum();
    let sum_min: f32 = col_min.iter().sum();
    // Available width: an explicit table width (clamped to the containing content width) else the
    // table shrinks to its preferred width, capped to the available content width.
    let avail = content.width;
    let mut col_w = col_pref.clone();
    let target = match table_cs.width {
        Some(w) => w.max(sum_min).min(avail.max(sum_min)),
        None => sum_pref.min(avail),
    };
    if target > sum_pref && sum_pref > 0.0 {
        // Grow columns proportionally to fill the target width.
        let extra = target - sum_pref;
        for c in 0..col_count {
            let share = if sum_pref > 0.0 { col_pref[c] / sum_pref } else { 1.0 / col_count as f32 };
            col_w[c] = col_pref[c] + extra * share;
        }
    } else if target < sum_pref {
        // Shrink columns toward min-content, distributing the deficit by shrinkable slack.
        let shrinkable: f32 = (sum_pref - sum_min).max(0.0);
        let deficit = sum_pref - target.max(sum_min);
        if shrinkable > 0.0 && deficit > 0.0 {
            for c in 0..col_count {
                let slack = col_pref[c] - col_min[c];
                let take = if shrinkable > 0.0 { deficit * (slack / shrinkable) } else { 0.0 };
                col_w[c] = (col_pref[c] - take).max(col_min[c]);
            }
        } else {
            col_w = col_min.clone();
        }
    }

    // Column x offsets (cumulative; separated borders model → no inter-column gap by default).
    let table_width: f32 = col_w.iter().sum();
    let mut col_x = vec![0.0f32; col_count + 1];
    for c in 0..col_count {
        col_x[c + 1] = col_x[c] + col_w[c];
    }

    // --- Caption above the grid. ---
    let caption_h = layout_table_captions(&mut captions, content, ctx, styles, measurer);
    let grid_top = content.y + caption_h;

    // --- 4. Measure each cell's content height at its column width. ---
    // A cell's content box width = sum of its spanned columns minus the cell's own h-edges.
    let mut measured_h: Vec<f32> = vec![0.0; cells.len()];
    for (i, cell) in cells.iter_mut().enumerate() {
        let start = cell.col;
        let end = (start + cell.colspan).min(col_count);
        let cell_border_w: f32 = col_w[start..end].iter().sum();
        let m = cell.boxx.dimensions.margin;
        let b = cell.boxx.dimensions.border;
        let p = cell.boxx.dimensions.padding;
        let h_edges = m.left + m.right + b.left + b.right + p.left + p.right;
        let cw = (cell_border_w - h_edges).max(0.0);
        cell.boxx.dimensions.content.x = content.x + col_x[start] + m.left + b.left + p.left;
        cell.boxx.dimensions.content.y = grid_top + m.top + b.top + p.top;
        cell.boxx.dimensions.content.width = cw;
        let laid = layout_flex_item_contents(&mut cell.boxx, ctx, styles, measurer);
        // Honor an explicit cell height as a floor.
        let explicit_h = style_of(&cell.boxx, styles).and_then(|cs| cs.height).unwrap_or(0.0);
        measured_h[i] = laid.max(explicit_h);
    }

    // --- Row heights = max cell (border-box) height; rowspan distributes to the last covered row. ---
    let mut row_h = vec![0.0f32; num_rows];
    for (i, cell) in cells.iter().enumerate() {
        let b = cell.boxx.dimensions.border;
        let p = cell.boxx.dimensions.padding;
        let m = cell.boxx.dimensions.margin;
        let v_edges = m.top + m.bottom + b.top + b.bottom + p.top + p.bottom;
        let total = measured_h[i] + v_edges;
        if cell.rowspan <= 1 {
            let r = cell.row.min(num_rows.saturating_sub(1));
            row_h[r] = row_h[r].max(total);
        } else {
            // Distribute: ensure the rows it covers sum to at least its height.
            let start = cell.row;
            let end = (start + cell.rowspan).min(num_rows);
            let have: f32 = row_h[start..end].iter().sum();
            if total > have {
                let last = end.saturating_sub(1).max(start);
                if last < num_rows {
                    row_h[last] += total - have;
                }
            }
        }
    }

    // Row y offsets.
    let mut row_y = vec![0.0f32; num_rows + 1];
    for r in 0..num_rows {
        row_y[r + 1] = row_y[r] + row_h[r];
    }
    let grid_h: f32 = row_h.iter().sum();

    // --- Final placement: each cell fills its spanned column/row rect. ---
    for cell in &mut cells {
        let start_c = cell.col;
        let end_c = (start_c + cell.colspan).min(col_count);
        let start_r = cell.row.min(num_rows.saturating_sub(1));
        let end_r = (cell.row + cell.rowspan).min(num_rows);
        let cell_border_w: f32 = col_w[start_c..end_c].iter().sum();
        let cell_border_h: f32 = row_h[start_r..end_r.max(start_r + 1)].iter().sum();
        let m = cell.boxx.dimensions.margin;
        let b = cell.boxx.dimensions.border;
        let p = cell.boxx.dimensions.padding;
        let h_edges = m.left + m.right + b.left + b.right + p.left + p.right;
        let v_edges = m.top + m.bottom + b.top + b.bottom + p.top + p.bottom;
        let cw = (cell_border_w - h_edges).max(0.0);
        let ch = (cell_border_h - v_edges).max(0.0);
        let cx = content.x + col_x[start_c] + m.left + b.left + p.left;
        let cy = grid_top + row_y[start_r] + m.top + b.top + p.top;
        cell.boxx.dimensions.content = Rect { x: cx, y: cy, width: cw, height: ch };
        // Re-lay the content into the (now taller) cell box. vertical-align defaults to top, so
        // content starts at the cell's content-box top (a documented simplification — middle/bottom
        // are not implemented).
        layout_flex_item_contents(&mut cell.boxx, ctx, styles, measurer);
    }

    // The table box is at least as wide as its columns. Record the used width.
    boxx.dimensions.content.width = table_width;

    // Rebuild the table box's children: captions first (above), then the flattened cells (the row /
    // row-group boxes were structural only — cells carry their own borders/backgrounds, so we drop
    // the wrappers and paint cells directly, mirroring how grid flattens its items).
    let mut new_children: Vec<LayoutBox> = Vec::with_capacity(captions.len() + cells.len());
    new_children.append(&mut captions);
    for cell in cells {
        new_children.push(cell.boxx);
    }
    boxx.children = new_children;

    caption_h + grid_h
}

/// Lay out a table's `<caption>` boxes full-width above the grid, stacked. Returns their total
/// height. Each caption is positioned at the table's content origin and laid out as a block.
fn layout_table_captions(
    captions: &mut [LayoutBox],
    content: Rect,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) -> f32 {
    let mut y = content.y;
    for cap in captions.iter_mut() {
        let containing = Rect { x: content.x, y, width: content.width, height: 0.0 };
        layout_block(cap, containing, ctx, styles, measurer);
        y += cap.dimensions.margin_box().height;
    }
    y - content.y
}

// ---------------------------------------------------------------------------------------------
// Grid layout (basic explicit-track, row-major placement)
// ---------------------------------------------------------------------------------------------

/// Lay out the grid items of `boxx`. Resolves `grid-template-columns`/`rows` into pixel tracks,
/// places items row-major into cells (honoring explicit `grid-column`/`grid-row` start lines and
/// spans where parsed), applies gaps, and positions each item's content within its cell.
/// Unsupported: named areas, auto-flow `dense`, implicit-track sizing beyond an equal share.
fn layout_grid(
    boxx: &mut LayoutBox,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) -> f32 {
    let cs = style_of(boxx, styles).cloned().unwrap_or_default();
    let content = boxx.dimensions.content;

    // In-flow item indices.
    let items: Vec<usize> = boxx
        .children
        .iter()
        .enumerate()
        .filter(|(_, c)| !is_out_of_flow(c, styles))
        .map(|(i, _)| i)
        .collect();

    // Column tracks: default to a single auto track if unspecified.
    let col_tracks = if cs.grid_template_columns.is_empty() {
        vec![style::TrackSize::Auto]
    } else {
        cs.grid_template_columns.clone()
    };
    let num_cols = col_tracks.len().max(1);

    // Determine number of rows: explicit rows, else enough to fit all items row-major.
    let explicit_rows = cs.grid_template_rows.clone();
    let num_rows = if !explicit_rows.is_empty() {
        explicit_rows.len()
    } else {
        items.len().div_ceil(num_cols).max(1)
    };

    // Resolve column widths in px.
    let col_widths = resolve_tracks(&col_tracks, content.width, cs.column_gap, num_cols);

    // Row heights: resolve explicit row tracks; for auto/implicit rows use a content estimate.
    let row_template = if explicit_rows.is_empty() {
        vec![style::TrackSize::Auto; num_rows]
    } else {
        // pad with Auto if fewer tracks than rows
        let mut v = explicit_rows.clone();
        while v.len() < num_rows {
            v.push(style::TrackSize::Auto);
        }
        v
    };

    // Place items into (row, col, col_span, row_span) cells, row-major.
    struct Placed {
        idx: usize,
        row: usize,
        col: usize,
        col_span: usize,
        row_span: usize,
    }
    let mut placed: Vec<Placed> = Vec::new();
    let mut cursor_r = 0usize;
    let mut cursor_c = 0usize;
    let mut occupied: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    for &i in &items {
        let child = &boxx.children[i];
        let ccs = style_of(child, styles).cloned().unwrap_or_default();
        // Explicit placement?
        let (col, col_span) = placement_to_cell(ccs.grid_column, num_cols);
        let (row_opt, row_span) = match ccs.grid_row {
            Some(p) => placement_to_cell(Some(p), num_rows.max(1)),
            None => (None, 1),
        };

        let (final_r, final_c) = if let Some(c) = col {
            // explicit column; row explicit or current cursor row
            let r = row_opt.unwrap_or(cursor_r);
            (r, c)
        } else {
            // auto-place: find next free cell row-major
            let mut r = cursor_r;
            let mut c = cursor_c;
            loop {
                if c + col_span > num_cols {
                    r += 1;
                    c = 0;
                }
                if !occupied.contains(&(r, c)) {
                    break;
                }
                c += 1;
            }
            (r, c)
        };

        for dr in 0..row_span {
            for dc in 0..col_span {
                occupied.insert((final_r + dr, final_c + dc));
            }
        }
        // Advance auto cursor past this item.
        cursor_r = final_r;
        cursor_c = final_c + col_span;
        if cursor_c >= num_cols {
            cursor_c = 0;
            cursor_r += 1;
        }

        placed.push(Placed {
            idx: i,
            row: final_r,
            col: final_c,
            col_span,
            row_span,
        });
    }

    // Recompute number of rows actually used.
    let used_rows = placed
        .iter()
        .map(|p| p.row + p.row_span)
        .max()
        .unwrap_or(num_rows)
        .max(num_rows);
    let mut row_tmpl = row_template;
    while row_tmpl.len() < used_rows {
        row_tmpl.push(style::TrackSize::Auto);
    }

    // Column x offsets (cumulative + gaps).
    let mut col_x = vec![0.0f32; num_cols + 1];
    for c in 0..num_cols {
        col_x[c + 1] = col_x[c] + col_widths[c] + if c + 1 < num_cols { cs.column_gap } else { 0.0 };
    }

    // Measure each placed item's real content height by laying its contents out at its actual cell
    // width. The single-line `intrinsic_cross_height` estimate used for auto rows badly
    // underestimates wrapped paragraphs and nested flex/block subtrees, which would let the row
    // collapse and following grid rows / siblings overlap. We capture the laid-out height here and
    // feed it into the auto-row sizing below. (Items are laid out again at their final cell rect
    // after row heights are known, so this is purely a measurement pass.)
    let mut measured_h: Vec<f32> = vec![0.0; boxx.children.len()];
    for p in &placed {
        let end_col = (p.col + p.col_span).min(num_cols);
        let mut w = 0.0;
        for (k, cw) in col_widths[p.col..end_col].iter().enumerate() {
            w += cw;
            if p.col + k + 1 < end_col {
                w += cs.column_gap;
            }
        }
        let child = &mut boxx.children[p.idx];
        let m = child.dimensions.margin;
        let b = child.dimensions.border;
        let pad = child.dimensions.padding;
        let edges_h = m.left + m.right + b.left + b.right + pad.left + pad.right;
        let ccs = style_of(child, styles).cloned().unwrap_or_default();
        let cw = ccs.width.unwrap_or((w - edges_h).max(0.0));
        child.dimensions.content.width = cw;
        let laid = layout_flex_item_contents(child, ctx, styles, measurer);
        measured_h[p.idx] = laid;
    }

    // Row heights: resolve. For auto rows, use an equal share of explicit container height if
    // set, else the tallest measured item (margin-box) in that row.
    let container_h = explicit_height(boxx, styles);
    let placed_rows: Vec<(usize, usize)> = placed.iter().map(|p| (p.row, p.idx)).collect();
    let row_heights = resolve_row_heights(
        &row_tmpl,
        container_h,
        cs.row_gap,
        used_rows,
        &placed_rows,
        boxx,
        styles,
        measurer,
        &measured_h,
    );

    let mut row_y = vec![0.0f32; used_rows + 1];
    for r in 0..used_rows {
        row_y[r + 1] = row_y[r] + row_heights[r] + if r + 1 < used_rows { cs.row_gap } else { 0.0 };
    }

    for p in &placed {
        // Cell rect: from start column/row to span.
        let cell_x = content.x + col_x[p.col.min(num_cols)];
        let cell_y = content.y + row_y[p.row.min(used_rows)];
        let end_col = (p.col + p.col_span).min(num_cols);
        let end_row = (p.row + p.row_span).min(used_rows);
        // Width = sum of spanned column tracks + internal gaps.
        let mut w = 0.0;
        for (k, cw) in col_widths[p.col..end_col].iter().enumerate() {
            w += cw;
            if p.col + k + 1 < end_col {
                w += cs.column_gap;
            }
        }
        let mut h = 0.0;
        for (k, rh) in row_heights[p.row..end_row].iter().enumerate() {
            h += rh;
            if p.row + k + 1 < end_row {
                h += cs.row_gap;
            }
        }

        let child = &mut boxx.children[p.idx];
        let ccs = style_of(child, styles).cloned().unwrap_or_default();
        let m = child.dimensions.margin;
        let b = child.dimensions.border;
        let pad = child.dimensions.padding;
        let edges_h = m.left + m.right + b.left + b.right + pad.left + pad.right;
        let edges_v = m.top + m.bottom + b.top + b.bottom + pad.top + pad.bottom;
        let cw = ccs.width.unwrap_or((w - edges_h).max(0.0));
        let ch = ccs.height.unwrap_or((h - edges_v).max(0.0));
        let cx = cell_x + m.left + b.left + pad.left;
        let cy = cell_y + m.top + b.top + pad.top;
        child.dimensions.content = Rect { x: cx, y: cy, width: cw, height: ch };
        layout_flex_item_contents(child, ctx, styles, measurer);
    }

    let total_h: f32 = row_heights.iter().sum::<f32>()
        + cs.row_gap * (used_rows.saturating_sub(1) as f32);
    container_h.unwrap_or(total_h)
}

/// Resolve a track list into pixel widths within `avail`, accounting for `gap` between tracks.
fn resolve_tracks(tracks: &[style::TrackSize], avail: f32, gap: f32, num: usize) -> Vec<f32> {
    let total_gap = gap * (num.saturating_sub(1) as f32);
    let space = (avail - total_gap).max(0.0);
    // Fixed (px + pct) consume space first; fr share the remainder; auto gets equal share of
    // whatever's left after fixed (treated like 1fr if any fr present, else equal split).
    let mut fixed = 0.0f32;
    let mut fr_total = 0.0f32;
    let mut auto_count = 0usize;
    for t in tracks {
        match t {
            style::TrackSize::Px(v) => fixed += v,
            style::TrackSize::Pct(p) => fixed += space * (p / 100.0),
            style::TrackSize::Fr(f) => fr_total += f,
            style::TrackSize::Auto => auto_count += 1,
        }
    }
    let remaining = (space - fixed).max(0.0);
    // Auto tracks take an equal share of remaining if no fr; if fr present, auto → 0 (min-content
    // approximated as 0 for simplicity).
    let auto_each = if fr_total == 0.0 && auto_count > 0 {
        remaining / auto_count as f32
    } else {
        0.0
    };
    let fr_unit = if fr_total > 0.0 {
        let after_auto = (remaining - auto_each * auto_count as f32).max(0.0);
        after_auto / fr_total
    } else {
        0.0
    };
    tracks
        .iter()
        .map(|t| match t {
            style::TrackSize::Px(v) => *v,
            style::TrackSize::Pct(p) => space * (p / 100.0),
            style::TrackSize::Fr(f) => fr_unit * f,
            style::TrackSize::Auto => auto_each,
        })
        .collect()
}

/// Resolve row heights. Px/Pct/Fr resolve against the container height if known; auto rows get a
/// content estimate (max intrinsic item height in the row) or an equal share.
#[allow(clippy::too_many_arguments)]
fn resolve_row_heights(
    tracks: &[style::TrackSize],
    container_h: Option<f32>,
    gap: f32,
    num: usize,
    placed: &[(usize, usize)], // (row, child_index)
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
    measured_h: &[f32], // per-child laid-out content height (0 if not measured)
) -> Vec<f32> {
    // Content estimate per row: max item content height among items starting in that row. Use the
    // greater of the single-line intrinsic estimate and the actually-laid-out height (which
    // accounts for wrapped text and nested flex/grid/block subtrees), plus the item's own vertical
    // box edges, so the row track is tall enough to contain its items.
    let mut content_rows = vec![0.0f32; num];
    for &(r, idx) in placed {
        if r < num {
            let child = &boxx.children[idx];
            let est = intrinsic_cross_height(child, styles, measurer);
            let m = child.dimensions.margin;
            let b = child.dimensions.border;
            let p = child.dimensions.padding;
            let v_edges = m.top + m.bottom + b.top + b.bottom + p.top + p.bottom;
            let measured = measured_h.get(idx).copied().unwrap_or(0.0) + v_edges;
            let h = est.max(measured);
            content_rows[r] = content_rows[r].max(h);
        }
    }

    if let Some(ch) = container_h {
        // Resolve px/pct/fr against ch; auto → content estimate.
        let total_gap = gap * (num.saturating_sub(1) as f32);
        let space = (ch - total_gap).max(0.0);
        let mut fixed = 0.0f32;
        let mut fr_total = 0.0f32;
        for (i, t) in tracks.iter().enumerate() {
            match t {
                style::TrackSize::Px(v) => fixed += v,
                style::TrackSize::Pct(p) => fixed += space * (p / 100.0),
                style::TrackSize::Fr(f) => fr_total += f,
                style::TrackSize::Auto => fixed += content_rows[i],
            }
        }
        let remaining = (space - fixed).max(0.0);
        let fr_unit = if fr_total > 0.0 { remaining / fr_total } else { 0.0 };
        tracks
            .iter()
            .enumerate()
            .map(|(i, t)| match t {
                style::TrackSize::Px(v) => *v,
                style::TrackSize::Pct(p) => space * (p / 100.0),
                style::TrackSize::Fr(f) => fr_unit * f,
                style::TrackSize::Auto => content_rows[i],
            })
            .collect()
    } else {
        // No container height: px/pct(→0) fixed, fr→content estimate, auto→content estimate.
        tracks
            .iter()
            .enumerate()
            .map(|(i, t)| match t {
                style::TrackSize::Px(v) => *v,
                style::TrackSize::Pct(_) => content_rows[i],
                style::TrackSize::Fr(_) => content_rows[i],
                style::TrackSize::Auto => content_rows[i],
            })
            .collect()
    }
}

/// Convert a `GridPlacement` into a 0-based `(start_col, span)` within `num` tracks. Auto start
/// returns `(None-like)` via the column being unknown — callers handle `None` separately, so here
/// we encode auto-start as returning column 0 with the caller checking `start`. To keep the
/// signature simple this returns `(Option<usize>, span)`.
fn placement_to_cell(p: Option<style::GridPlacement>, num: usize) -> (Option<usize>, usize) {
    match p {
        None => (None, 1),
        Some(pl) => {
            let span = match pl.end {
                style::GridEnd::Span(s) => (s.max(1)) as usize,
                style::GridEnd::Line(e) => {
                    if let Some(s) = pl.start {
                        ((e - s).max(1)) as usize
                    } else {
                        1
                    }
                }
                style::GridEnd::Auto => 1,
            };
            let start = pl.start.map(|s| {
                // 1-based line → 0-based track index.
                let idx = (s - 1).max(0) as usize;
                idx.min(num.saturating_sub(1))
            });
            (start, span.min(num.max(1)))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Inline layout (line boxes + text wrapping)
// ---------------------------------------------------------------------------------------------

/// Lay out the inline/text children of `boxx` into line boxes, replacing `boxx.children` with a
/// flat list of positioned `Text` boxes (one per wrapped line per run) plus any atomic
/// inline-block boxes positioned on their line. Returns total height. `align` is the text
/// alignment of the block establishing this inline context.
fn layout_inline_children(
    boxx: &mut LayoutBox,
    align: TextAlignLocal,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) -> f32 {
    let content = boxx.dimensions.content;
    let avail = content.width.max(0.0);

    // Flatten inline content into a sequence of inline items: words and atomic inline-blocks.
    // We move children out so atomic boxes can be re-emitted into the new child list.
    let original_children = std::mem::take(&mut boxx.children);
    let mut items: Vec<InlineItem> = Vec::new();
    collect_inline_items(original_children, ctx, styles, measurer, &mut items);

    // Greedy line breaking. Each line is a list of (item, x-offset-within-line).
    struct PlacedLine {
        items: Vec<(InlineItem, f32)>, // item + x offset within line
        width: f32,
        height: f32,
        max_font_size: f32,
    }
    let mut lines: Vec<PlacedLine> = Vec::new();
    let mut cur: Vec<(InlineItem, f32)> = Vec::new();
    let mut cursor_x = 0.0f32;
    let mut max_fs = 0.0f32;
    let mut line_h = 0.0f32;

    for item in items {
        // A forced break (`<br>` / preserved `\n`) ends the current line unconditionally and starts
        // a fresh one — even when the line is empty (so consecutive breaks produce blank lines).
        if let InlineItem::Break { font_size } = &item {
            let h = if line_h > 0.0 { line_h } else { measurer.line_height(*font_size) };
            let fs = if max_fs > 0.0 { max_fs } else { *font_size };
            lines.push(PlacedLine {
                items: std::mem::take(&mut cur),
                width: cursor_x,
                height: h,
                max_font_size: fs,
            });
            cursor_x = 0.0;
            max_fs = 0.0;
            line_h = 0.0;
            continue;
        }
        let (w, fs, h, leads_space) = item.metrics(measurer);
        let space_w = if cur.is_empty() || !leads_space {
            0.0
        } else {
            measurer.text_width(" ", fs, false)
        };
        if !cur.is_empty() && cursor_x + space_w + w > avail {
            lines.push(PlacedLine {
                items: std::mem::take(&mut cur),
                width: cursor_x,
                height: line_h,
                max_font_size: max_fs,
            });
            cursor_x = 0.0;
            max_fs = 0.0;
            line_h = 0.0;
        }
        if !cur.is_empty() {
            cursor_x += space_w;
        }
        max_fs = max_fs.max(fs);
        line_h = line_h.max(h);
        cur.push((item, cursor_x));
        cursor_x += w;
    }
    if !cur.is_empty() {
        lines.push(PlacedLine {
            items: cur,
            width: cursor_x,
            height: line_h,
            max_font_size: max_fs,
        });
    }

    // Emit positioned boxes per line.
    let mut new_children: Vec<LayoutBox> = Vec::new();
    let mut y = content.y;
    let mut total_h = 0.0f32;
    for line in &lines {
        let line_font = if line.max_font_size > 0.0 { line.max_font_size } else { 16.0 };
        // The line advance is the tallest item's preferred line-height (its computed
        // `line-height` if set, else the font metric — both already folded into `line.height`).
        let lh = if line.height > 0.0 { line.height } else { measurer.line_height(line_font) };
        // The emitted Text box's own height matches the line advance.
        let text_lh = lh;
        let line_x = match align {
            TextAlignLocal::Left => content.x,
            TextAlignLocal::Center => content.x + (avail - line.width).max(0.0) / 2.0,
            TextAlignLocal::Right => content.x + (avail - line.width).max(0.0),
        };

        // Words on this line: emit a `Text` box per contiguous run of words sharing the same
        // DOM node, so painted text can be traced back to its element (e.g. an `<a>`). Each run's
        // box is positioned at the run's starting x offset within the line (the offset of its
        // first word, which already accounts for inter-word/inter-run spacing). Visual output is
        // unchanged: the same words at the same positions, only split where the source node
        // changes. Atomic boxes are repositioned at their offset as before.
        //
        // A "run" accumulates the words' joined text, the run's start x offset (line-relative),
        // its node, the paint style of its first word, and the font size to measure at.
        struct Run {
            texts: Vec<String>,
            start_off: f32,
            node: Option<dom::NodeId>,
            style: PaintStyle,
        }
        let mut run: Option<Run> = None;
        let flush = |run: &mut Option<Run>, out: &mut Vec<LayoutBox>| {
            if let Some(r) = run.take() {
                let text = r.texts.join(" ");
                let ls = r.style.letter_spacing;
                // `vertical-align: sub|super` shifts the run off the line's baseline by ~0.3em
                // (of the run's own, already-reduced, font size). Super raises (smaller y), sub
                // lowers (larger y). Width/height are measured at the run's own font size so the
                // shifted sub/sup text keeps its reduced size.
                let run_fs = r.style.font_size;
                let voff = match r.style.vertical_align {
                    style::VerticalAlign::Super => -run_fs * 0.3,
                    style::VerticalAlign::Sub => run_fs * 0.3,
                    style::VerticalAlign::Baseline => 0.0,
                };
                let measure_fs = if run_fs > 0.0 { run_fs } else { line_font };
                let mut tb = LayoutBox::new(BoxContent::Text(text), r.style, r.node);
                let w = run_width(measurer, &tb_text(&tb), measure_fs, false, ls);
                tb.dimensions.content =
                    Rect { x: line_x + r.start_off, y: y + voff, width: w, height: text_lh };
                out.push(tb);
            }
        };
        for (item, off) in &line.items {
            match item {
                InlineItem::Word { text, style, node, .. } => {
                    match &mut run {
                        // Continue the current run only if the node matches.
                        Some(r) if r.node == *node => r.texts.push(text.clone()),
                        _ => {
                            flush(&mut run, &mut new_children);
                            run = Some(Run {
                                texts: vec![text.clone()],
                                start_off: *off,
                                node: *node,
                                style: style.clone(),
                            });
                        }
                    }
                }
                InlineItem::Break { .. } => {
                    // Breaks are consumed during line building and never placed onto a line.
                    flush(&mut run, &mut new_children);
                }
                InlineItem::Atomic(b) => {
                    // An atomic box interrupts any word run.
                    flush(&mut run, &mut new_children);
                    let mut ab = (**b).clone();
                    // Reposition so the atomic box's margin-box sits at (line_x + off, y).
                    let mb = ab.dimensions.margin_box();
                    let dx = (line_x + off) - mb.x;
                    let dy = y - mb.y;
                    shift_subtree(&mut ab, dx, dy);
                    new_children.push(ab);
                }
            }
        }
        flush(&mut run, &mut new_children);
        y += lh;
        total_h += lh;
    }

    boxx.children = new_children;
    let _ = ctx;
    total_h
}

/// Advance width of a text run including `letter-spacing` (added once per character).
fn run_width(measurer: &dyn TextMeasurer, text: &str, px: f32, bold: bool, letter_spacing: f32) -> f32 {
    let base = measurer.text_width(text, px, bold);
    if letter_spacing != 0.0 {
        base + letter_spacing * text.chars().count() as f32
    } else {
        base
    }
}

/// Helper to read the text out of a Text box (for measuring).
fn tb_text(b: &LayoutBox) -> String {
    match &b.content {
        BoxContent::Text(t) => t.clone(),
        _ => String::new(),
    }
}

/// A single word with its paint style, ready for line breaking.
struct InlineWord {
    text: String,
    style: PaintStyle,
    /// The DOM node of the source text box this word came from. Carried for parity with
    /// `InlineItem::Word`; the intrinsic-sizing path that builds `InlineWord`s doesn't read it.
    #[allow(dead_code)]
    node: Option<dom::NodeId>,
}

/// An inline-level item participating in line layout: either a word or an atomic inline-block.
enum InlineItem {
    /// A word carrying the DOM node of its source text box (used for hit-testing). `leads_space` is
    /// whether an inter-word space precedes it on a line (true for normal words; false for a
    /// `white-space: pre` run, whose spaces are already inside `text`).
    Word { text: String, style: PaintStyle, node: Option<dom::NodeId>, leads_space: bool },
    /// An atomic box (inline-block / inline-flex / inline-grid) already laid out at a tentative
    /// origin; it advances the pen by its margin-box width and is repositioned on its line.
    Atomic(Box<LayoutBox>),
    /// A forced line break (`<br>` or a preserved `\n`): ends the current line box. `font_size` lets
    /// an empty break line still advance by a sensible line height.
    Break { font_size: f32 },
}

impl InlineItem {
    /// Returns (advance_width, font_size, height, leads_with_space). `height` is the item's
    /// preferred line advance: the element's computed `line-height` if set, else the font metric.
    fn metrics(&self, measurer: &dyn TextMeasurer) -> (f32, f32, f32, bool) {
        match self {
            InlineItem::Word { text, style, leads_space, .. } => {
                let w = run_width(measurer, text, style.font_size, style.bold, style.letter_spacing);
                let lh = style.line_height.unwrap_or_else(|| measurer.line_height(style.font_size));
                (w, style.font_size, lh, *leads_space)
            }
            InlineItem::Atomic(b) => {
                let mb = b.dimensions.margin_box();
                (mb.width, b.style.font_size, mb.height, false)
            }
            InlineItem::Break { font_size } => (0.0, *font_size, measurer.line_height(*font_size), false),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum TextAlignLocal {
    Left,
    Center,
    Right,
}

/// Recursively collect inline items from an inline subtree (consuming the boxes). Text boxes
/// contribute words; inline elements recurse; inline-block / inline-flex / inline-grid boxes
/// become atomic items (already laid out at a tentative origin by `make_atomic`).
fn collect_inline_items(
    children: Vec<LayoutBox>,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
    out: &mut Vec<InlineItem>,
) {
    for mut child in children {
        // An element whose display is inline-block (or inline-flex/grid) is atomic.
        let is_atomic = child.node.and_then(|n| styles.get(&n)).map(|cs| {
            matches!(
                cs.display,
                style::Display::InlineBlock
                    | style::Display::InlineFlex
                    | style::Display::InlineGrid
            )
        }) == Some(true);

        if is_atomic {
            // Lay the atomic box out as a block at a tentative origin (0,0). Its intrinsic width
            // (max content line, or explicit width) becomes its size; it'll be repositioned on
            // its line. We use the box's intrinsic width as the containing width so shrink-to-fit
            // applies.
            let m = child.dimensions.margin;
            let iw = intrinsic_width(&child, styles, measurer) + m.left + m.right;
            let containing = Rect { x: 0.0, y: 0.0, width: iw, height: 0.0 };
            layout_block(&mut child, containing, ctx, styles, measurer);
            out.push(InlineItem::Atomic(Box::new(child)));
            continue;
        }
        match &child.content {
            BoxContent::Text(text) => {
                // Carry the source text box's DOM node onto each word so emitted line `Text`
                // boxes can be traced back to their element for hit-testing.
                let node = child.node;
                if child.style.white_space.preserves_spaces() {
                    // `white-space: pre`/`pre-wrap`: the run is atomic — spaces are PRESERVED (no
                    // split) and no inter-word space precedes it (its spaces are inside `text`).
                    // Newlines were already split into separate runs + `LineBreak`s at build time.
                    if !text.is_empty() {
                        out.push(InlineItem::Word {
                            text: text.clone(),
                            style: child.style.clone(),
                            node,
                            leads_space: false,
                        });
                    }
                } else {
                    for word in text.split_whitespace() {
                        out.push(InlineItem::Word {
                            text: word.to_string(),
                            style: child.style.clone(),
                            node,
                            leads_space: true,
                        });
                    }
                }
            }
            BoxContent::LineBreak => {
                out.push(InlineItem::Break { font_size: child.style.font_size });
            }
            BoxContent::Inline => {
                collect_inline_items(child.children, ctx, styles, measurer, out);
            }
            BoxContent::Image(_) => {
                // An atomic inline image: position its (pre-sized) content box at a tentative
                // origin so its margin box is well-formed, then emit it as an atomic item. It
                // advances the line by its margin-box width and is repositioned on its line.
                let containing = Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 };
                layout_image_box(&mut child, containing);
                out.push(InlineItem::Atomic(Box::new(child)));
            }
            BoxContent::Caret => {
                // The focused-field caret: an atomic, pre-sized thin bar. Like an image, give it a
                // well-formed margin box at a tentative origin, then flow it inline so it sits
                // right after the value text (its top margin centers it on the line).
                let containing = Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 };
                layout_image_box(&mut child, containing);
                out.push(InlineItem::Atomic(Box::new(child)));
            }
            BoxContent::Widget(_) => {
                // A drawn form widget: pre-sized (content set at build time), replaced-like. Treat
                // it as an atomic inline box (like an image) so it advances the line by its
                // border-box width and is repositioned on its line.
                let containing = Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 };
                layout_image_box(&mut child, containing);
                out.push(InlineItem::Atomic(Box::new(child)));
            }
            _ => {
                // Block-level content shouldn't appear in an inline context; ignore defensively.
            }
        }
    }
}

/// Recursively collect words from an inline subtree (used by intrinsic sizing). Inline element
/// boxes contribute their children's words.
fn collect_inline_words(children: &[LayoutBox], out: &mut Vec<InlineWord>) {
    for child in children {
        match &child.content {
            BoxContent::Text(text) => {
                for word in text.split_whitespace() {
                    out.push(InlineWord {
                        text: word.to_string(),
                        style: child.style.clone(),
                        node: child.node,
                    });
                }
            }
            BoxContent::Inline => {
                collect_inline_words(&child.children, out);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A stub measurer: each char is `0.6 * px` wide; line height is `1.3 * px`.
    struct Stub;
    impl TextMeasurer for Stub {
        fn text_width(&self, text: &str, px: f32, _bold: bool) -> f32 {
            text.chars().count() as f32 * px * 0.6
        }
        fn line_height(&self, px: f32) -> f32 {
            px * 1.3
        }
    }

    /// Build a styled document: returns the doc plus the computed-style map. `setup` populates
    /// the DOM and returns nothing; styles are supplied directly per node id.
    fn block_style(display_block: bool) -> style::ComputedStyle {
        style::ComputedStyle { display_block, ..Default::default() }
    }

    /// Find the first descendant box (DFS) matching `pred`.
    fn find_box<'a>(b: &'a LayoutBox, pred: &dyn Fn(&LayoutBox) -> bool) -> Option<&'a LayoutBox> {
        if pred(b) {
            return Some(b);
        }
        for c in &b.children {
            if let Some(f) = find_box(c, pred) {
                return Some(f);
            }
        }
        None
    }

    fn count_boxes(b: &LayoutBox, pred: &dyn Fn(&LayoutBox) -> bool) -> usize {
        let mut n = if pred(b) { 1 } else { 0 };
        for c in &b.children {
            n += count_boxes(c, pred);
        }
        n
    }

    /// Collect every Text box (DFS) into a flat list.
    fn collect_text_box_list(b: &LayoutBox) -> Vec<&LayoutBox> {
        let mut v = Vec::new();
        fn go<'a>(b: &'a LayoutBox, v: &mut Vec<&'a LayoutBox>) {
            if matches!(b.content, BoxContent::Text(_)) {
                v.push(b);
            }
            for c in &b.children {
                go(c, v);
            }
        }
        go(b, &mut v);
        v
    }

    #[test]
    fn br_splits_text_onto_two_lines() {
        // body > p > "first" <br> "second"
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let p = doc.append_element(body, "p");
        doc.append_child(p, dom::NodeData::Text("first".into()));
        let br = doc.append_element(p, "br");
        doc.append_child(p, dom::NodeData::Text("second".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(p, block_style(true));
        // <br> is inline; the build path special-cases the tag, so any style works.
        styles.insert(br, style::ComputedStyle::default());

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let texts = collect_text_box_list(&root_box);
        let first = texts.iter().find(|b| matches!(&b.content, BoxContent::Text(t) if t == "first")).unwrap();
        let second = texts.iter().find(|b| matches!(&b.content, BoxContent::Text(t) if t == "second")).unwrap();
        // The <br> forces "second" onto the next line: strictly greater y.
        assert!(
            second.dimensions.content.y > first.dimensions.content.y,
            "second.y={} should be below first.y={}",
            second.dimensions.content.y,
            first.dimensions.content.y
        );
        // And both start at the same x (left edge), confirming it's a line break, not a wrap mid-line.
        assert_eq!(first.dimensions.content.x, second.dimensions.content.x);
    }

    #[test]
    fn pre_preserves_spaces_and_newline() {
        // body > pre  with text "a   b\nc": 3 spaces preserved, newline → two lines.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let pre = doc.append_element(body, "pre");
        doc.append_child(pre, dom::NodeData::Text("a   b\nc".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            pre,
            style::ComputedStyle {
                display_block: true,
                white_space: style::WhiteSpace::Pre,
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let texts = collect_text_box_list(&root_box);
        // Two runs: "a   b" (line 1) and "c" (line 2).
        let l1 = texts.iter().find(|b| matches!(&b.content, BoxContent::Text(t) if t == "a   b")).expect("first pre line preserved with 3 spaces");
        let l2 = texts.iter().find(|b| matches!(&b.content, BoxContent::Text(t) if t == "c")).expect("second pre line after the newline");
        // The newline put them on different lines.
        assert!(l2.dimensions.content.y > l1.dimensions.content.y, "newline should drop 'c' to a new line");
        // The first run's width reflects the preserved spaces: "a   b" = 5 chars at 0.6*16.
        let expected = Stub.text_width("a   b", 16.0, false);
        assert!((l1.dimensions.content.width - expected).abs() < 0.01, "width {} != {}", l1.dimensions.content.width, expected);
    }

    #[test]
    fn ul_generates_bullet_markers_and_ol_numbers() {
        // body > ul > li,li   and   body > ol > li,li
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let ul = doc.append_element(body, "ul");
        let li1 = doc.append_element(ul, "li");
        doc.append_child(li1, dom::NodeData::Text("one".into()));
        let li2 = doc.append_element(ul, "li");
        doc.append_child(li2, dom::NodeData::Text("two".into()));
        let ol = doc.append_element(body, "ol");
        let oli1 = doc.append_element(ol, "li");
        doc.append_child(oli1, dom::NodeData::Text("x".into()));
        let oli2 = doc.append_element(ol, "li");
        doc.append_child(oli2, dom::NodeData::Text("y".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        // ul: disc markers; padding-left 40 like the UA sheet.
        let mut ul_s = block_style(true);
        ul_s.list_style_type = style::ListStyleType::Disc;
        ul_s.padding = style::Edges { left: 40.0, ..Default::default() };
        styles.insert(ul, ul_s);
        // ol: decimal markers (li inherit list_style_type).
        let mut ol_s = block_style(true);
        ol_s.list_style_type = style::ListStyleType::Decimal;
        ol_s.padding = style::Edges { left: 40.0, ..Default::default() };
        styles.insert(ol, ol_s);
        for (id, lst) in [(li1, style::ListStyleType::Disc), (li2, style::ListStyleType::Disc), (oli1, style::ListStyleType::Decimal), (oli2, style::ListStyleType::Decimal)] {
            let mut s = block_style(true);
            s.list_style_type = lst;
            styles.insert(id, s);
        }

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        // Collect markers.
        fn markers(b: &LayoutBox, out: &mut Vec<String>) {
            if let BoxContent::Marker(s) = &b.content {
                out.push(s.to_string());
            }
            for c in &b.children {
                markers(c, out);
            }
        }
        let mut ms = Vec::new();
        markers(&root_box, &mut ms);
        // Two bullets (•) for the ul, then "1." and "2." for the ol.
        assert!(ms.iter().filter(|m| *m == "\u{2022}").count() == 2, "expected two disc bullets, got {ms:?}");
        assert!(ms.contains(&"1.".to_string()) && ms.contains(&"2.".to_string()), "expected 1. and 2. ol markers, got {ms:?}");
        // The first ul li's marker sits to the LEFT of the li content (in the 40px padding).
        let li1_box = find_box(&root_box, &|x| x.node == Some(li1) && matches!(x.content, BoxContent::Block)).unwrap();
        let marker_box = find_box(&root_box, &|x| matches!(x.content, BoxContent::Marker(_)) && x.node == Some(li1)).unwrap();
        assert!(marker_box.dimensions.content.x < li1_box.dimensions.content.x, "marker should be left of li content");
    }

    #[test]
    fn two_stacked_blocks_increasing_y_and_heights() {
        // body > div#a (height 30), div#b (height 50)
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let a = doc.append_element(body, "div");
        let b = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(a, style::ComputedStyle { display_block: true, height: Some(30.0), ..Default::default() });
        styles.insert(b, style::ComputedStyle { display_block: true, height: Some(50.0), ..Default::default() });

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        let bbox = find_box(&root_box, &|x| x.node == Some(b)).unwrap();
        assert_eq!(abox.dimensions.content.y, 0.0);
        assert_eq!(abox.dimensions.content.height, 30.0);
        // b stacks below a (a's margin box height = 30).
        assert_eq!(bbox.dimensions.content.y, 30.0);
        assert_eq!(bbox.dimensions.content.height, 50.0);
    }

    #[test]
    fn padding_and_border_offset_content_rect() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let a = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            a,
            style::ComputedStyle {
                display_block: true,
                height: Some(20.0),
                padding: style::Edges::all(5.0),
                border: style::Edges::all(2.0),
                margin: style::Edges::all(10.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        let c = abox.dimensions.content;
        // content x = 0 (containing) + margin.left 10 + border.left 2 + padding.left 5 = 17.
        assert_eq!(c.x, 17.0);
        assert_eq!(c.y, 17.0);
        // border box = content expanded by padding (5) + border (2): origin shifts by 7.
        let bb = abox.dimensions.border_box();
        assert_eq!(bb.x, 10.0); // content.x 17 - padding 5 - border 2
        // margin box origin is at the containing origin.
        let mb = abox.dimensions.margin_box();
        assert_eq!(mb.x, 0.0);
        assert_eq!(mb.y, 0.0);
        // content width = 800 - (margin 20 + border 4 + padding 10) = 766.
        assert_eq!(c.width, 766.0);
    }

    #[test]
    fn max_width_clamps_box_in_wide_container() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let a = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            a,
            style::ComputedStyle {
                display_block: true,
                max_width: Some(style::SizeConstraint::Px(200.0)),
                ..Default::default()
            },
        );

        // Container is 1000 wide; the box must be clamped to 200.
        let root_box = layout_document(&doc, &styles, 1000.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        assert_eq!(abox.dimensions.content.width, 200.0);
    }

    #[test]
    fn min_width_raises_small_box() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let a = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            a,
            style::ComputedStyle {
                display_block: true,
                width: Some(50.0),
                min_width: Some(style::SizeConstraint::Px(120.0)),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        // min-width raises the 50px width to 120.
        assert_eq!(abox.dimensions.content.width, 120.0);
    }

    #[test]
    fn max_height_clamps_box_height() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let a = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            a,
            style::ComputedStyle {
                display_block: true,
                height: Some(300.0),
                max_height: Some(style::SizeConstraint::Px(100.0)),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        assert_eq!(abox.dimensions.content.height, 100.0);
    }

    #[test]
    fn text_transform_uppercases_text_box_content() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let p = doc.append_element(body, "p");
        doc.append_child(p, dom::NodeData::Text("hello world".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            p,
            style::ComputedStyle {
                display_block: true,
                text_transform: style::TextTransform::Uppercase,
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(p)).unwrap();
        let texts = collect_text_boxes(pbox);
        let joined: String = texts
            .iter()
            .filter_map(|b| match &b.content {
                BoxContent::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("HELLO"), "got {joined:?}");
        assert!(joined.contains("WORLD"), "got {joined:?}");
    }

    #[test]
    fn line_height_changes_line_advance() {
        // A single line of text with line-height 40px → the block's content height is 40 (one
        // line), versus the default ~20.8 font metric.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let p = doc.append_element(body, "p");
        doc.append_child(p, dom::NodeData::Text("one".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            p,
            style::ComputedStyle {
                display_block: true,
                line_height: Some(40.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(p)).unwrap();
        assert!((pbox.dimensions.content.height - 40.0).abs() < 0.01,
            "expected line advance 40, got {}", pbox.dimensions.content.height);
    }

    #[test]
    fn explicit_width_is_respected() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let a = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(a, style::ComputedStyle { display_block: true, width: Some(200.0), ..Default::default() });

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        assert_eq!(abox.dimensions.content.width, 200.0);
    }

    #[test]
    fn text_wraps_to_multiple_lines_at_narrow_width() {
        // A paragraph with several words; narrow width forces wrapping.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let p = doc.append_element(body, "p");
        doc.append_child(p, dom::NodeData::Text("one two three four five six".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(p, block_style(true)); // font_size 16 default

        // word "three" = 5 chars * 16 * 0.6 = 48px. Width 60 fits ~one word per line.
        let root_box = layout_document(&doc, &styles, 60.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(p)).unwrap();
        let lines = count_boxes(pbox, &|x| matches!(x.content, BoxContent::Text(_)));
        assert!(lines > 1, "expected multiple wrapped lines, got {lines}");
        // Total height = lines * line_height(16) = lines * 20.8.
        let expected_h = lines as f32 * 16.0 * 1.3;
        assert!((pbox.dimensions.content.height - expected_h).abs() < 0.01);
    }

    #[test]
    fn inline_anchor_text_box_carries_node() {
        // p > "foo " <a>"bar baz qux"</a> " end"  — the <a> wraps; its emitted line Text box(es)
        // must carry the <a>'s text node id so clicks map back to the link.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let p = doc.append_element(body, "p");
        doc.append_child(p, dom::NodeData::Text("foo ".into()));
        let a = doc.append_element(p, "a");
        let a_text = doc.append_child(a, dom::NodeData::Text("bar baz qux".into()));
        doc.append_child(p, dom::NodeData::Text(" end".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(p, block_style(true));
        // <a> is inline by default.
        styles.insert(a, style::ComputedStyle::default());

        // Narrow width forces wrapping so we exercise multi-line runs.
        let root_box = layout_document(&doc, &styles, 80.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(p)).unwrap();

        // Some emitted Text box must carry the <a>'s text node.
        let carries_a_text =
            count_boxes(pbox, &|x| matches!(x.content, BoxContent::Text(_)) && x.node == Some(a_text));
        assert!(
            carries_a_text >= 1,
            "expected at least one Text box carrying the <a>'s text node id"
        );

        // The <a>'s text boxes only contain the anchor's words (no "foo"/"end" leakage).
        for tb in collect_text_boxes(pbox) {
            if tb.node == Some(a_text) {
                if let BoxContent::Text(t) = &tb.content {
                    for w in t.split_whitespace() {
                        assert!(
                            ["bar", "baz", "qux"].contains(&w),
                            "anchor text run leaked non-anchor word: {w}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn vertical_align_sub_super_offsets_the_run() {
        // p > "base" <sup>"hi"</sup> <sub>"lo"</sub> — the superscript run sits above the base
        // run's y, the subscript run below it.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let p = doc.append_element(body, "p");
        doc.append_child(p, dom::NodeData::Text("base".into()));
        let sup = doc.append_element(p, "sup");
        let sup_text = doc.append_child(sup, dom::NodeData::Text("hi".into()));
        let sub = doc.append_element(p, "sub");
        let sub_text = doc.append_child(sub, dom::NodeData::Text("lo".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(p, block_style(true));
        let mut sup_cs = style::ComputedStyle::default();
        sup_cs.vertical_align = style::VerticalAlign::Super;
        let mut sub_cs = style::ComputedStyle::default();
        sub_cs.vertical_align = style::VerticalAlign::Sub;
        styles.insert(sup, sup_cs);
        styles.insert(sub, sub_cs);

        let root_box = layout_document(&doc, &styles, 400.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(p)).unwrap();

        let y_of = |node: dom::NodeId| -> f32 {
            find_box(pbox, &|x| matches!(x.content, BoxContent::Text(_)) && x.node == Some(node))
                .unwrap()
                .dimensions
                .content
                .y
        };
        let base_y =
            find_box(pbox, &|x| matches!(&x.content, BoxContent::Text(t) if t == "base"))
                .unwrap()
                .dimensions
                .content
                .y;
        let sup_y = y_of(sup_text);
        let sub_y = y_of(sub_text);
        assert!(sup_y < base_y, "sup run ({sup_y}) should sit above base ({base_y})");
        assert!(sub_y > base_y, "sub run ({sub_y}) should sit below base ({base_y})");
    }

    /// Collect references to all `Text` boxes in a subtree (DFS).
    fn collect_text_boxes(b: &LayoutBox) -> Vec<&LayoutBox> {
        let mut out = Vec::new();
        fn go<'a>(b: &'a LayoutBox, out: &mut Vec<&'a LayoutBox>) {
            if matches!(b.content, BoxContent::Text(_)) {
                out.push(b);
            }
            for c in &b.children {
                go(c, out);
            }
        }
        go(b, &mut out);
        out
    }

    #[test]
    fn display_none_produces_no_box() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let hidden = doc.append_element(body, "div");
        let shown = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(hidden, style::ComputedStyle { display_block: true, display_none: true, ..Default::default() });
        styles.insert(shown, block_style(true));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        assert!(find_box(&root_box, &|x| x.node == Some(hidden)).is_none());
        assert!(find_box(&root_box, &|x| x.node == Some(shown)).is_some());
    }

    #[test]
    fn anonymous_box_wraps_mixed_inline_among_blocks() {
        // body > [ text, div ] : the leading text must be wrapped in an anonymous block.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        doc.append_child(body, dom::NodeData::Text("hello".into()));
        let d = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(d, block_style(true));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let anon = count_boxes(&root_box, &|x| matches!(x.content, BoxContent::Anonymous));
        assert_eq!(anon, 1);
    }

    #[test]
    fn deeply_nested_does_not_panic() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let mut styles = HashMap::new();
        let mut parent = doc.append_element(root, "body");
        styles.insert(parent, block_style(true));
        // A few hundred levels of nesting (more than any reasonable page) on a normal stack.
        for _ in 0..400 {
            let child = doc.append_element(parent, "div");
            styles.insert(child, block_style(true));
            parent = child;
        }
        doc.append_child(parent, dom::NodeData::Text("deep".into()));
        let _ = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
    }

    #[test]
    fn border_box_expands_content_by_padding_and_border() {
        let d = Dimensions {
            content: Rect { x: 10.0, y: 10.0, width: 100.0, height: 20.0 },
            padding: Edges { top: 5.0, right: 5.0, bottom: 5.0, left: 5.0 },
            border: Edges { top: 2.0, right: 2.0, bottom: 2.0, left: 2.0 },
            margin: Edges::default(),
        };
        let b = d.border_box();
        assert_eq!(b.x, 3.0);
        assert_eq!(b.width, 100.0 + 14.0);
    }

    // ----------------------------------------------------------------------------------------
    // Flex / positioning / inline-block / grid
    // ----------------------------------------------------------------------------------------

    /// A flex-container style with the given direction.
    fn flex_container(dir: style::FlexDirection) -> style::ComputedStyle {
        style::ComputedStyle {
            display: style::Display::Flex,
            display_block: true,
            flex_direction: dir,
            ..Default::default()
        }
    }

    #[test]
    fn flex_row_space_between_anchors_first_and_last() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let c = doc.append_element(body, "div");
        let a = doc.append_element(c, "div");
        let b = doc.append_element(c, "div");
        let d = doc.append_element(c, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            c,
            style::ComputedStyle {
                display: style::Display::Flex,
                display_block: true,
                width: Some(300.0),
                justify_content: style::JustifyContent::SpaceBetween,
                ..Default::default()
            },
        );
        let item = |w: f32| style::ComputedStyle {
            display: style::Display::Block,
            display_block: true,
            width: Some(w),
            height: Some(20.0),
            ..Default::default()
        };
        styles.insert(a, item(50.0));
        styles.insert(b, item(50.0));
        styles.insert(d, item(50.0));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        let dbox = find_box(&root_box, &|x| x.node == Some(d)).unwrap();
        let cbox = find_box(&root_box, &|x| x.node == Some(c)).unwrap();
        // First item at the container's left content edge.
        assert!((abox.dimensions.content.x - cbox.dimensions.content.x).abs() < 0.01);
        // Last item's right edge flush with the container's right content edge.
        let last_right = dbox.dimensions.content.x + dbox.dimensions.content.width;
        let cont_right = cbox.dimensions.content.x + 300.0;
        assert!((last_right - cont_right).abs() < 0.01, "last_right={last_right} cont_right={cont_right}");
    }

    #[test]
    fn flex_grow_expands_middle_child() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let c = doc.append_element(body, "div");
        let a = doc.append_element(c, "div");
        let b = doc.append_element(c, "div");
        let d = doc.append_element(c, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            c,
            style::ComputedStyle {
                display: style::Display::Flex,
                display_block: true,
                width: Some(300.0),
                ..Default::default()
            },
        );
        let fixed = style::ComputedStyle {
            display: style::Display::Block,
            display_block: true,
            width: Some(50.0),
            ..Default::default()
        };
        styles.insert(a, fixed.clone());
        styles.insert(
            b,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                flex_grow: 1.0,
                flex_basis: Some(0.0),
                ..Default::default()
            },
        );
        styles.insert(d, fixed);

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let bbox = find_box(&root_box, &|x| x.node == Some(b)).unwrap();
        // free = 300 - 50 - 50 - 0(basis) = 200, all goes to b.
        assert!((bbox.dimensions.content.width - 200.0).abs() < 0.01,
            "got {}", bbox.dimensions.content.width);
    }

    #[test]
    fn flex_column_stacks_vertically() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let c = doc.append_element(body, "div");
        let a = doc.append_element(c, "div");
        let b = doc.append_element(c, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(c, flex_container(style::FlexDirection::Column));
        let item = |h: f32| style::ComputedStyle {
            display: style::Display::Block,
            display_block: true,
            height: Some(h),
            width: Some(40.0),
            ..Default::default()
        };
        styles.insert(a, item(30.0));
        styles.insert(b, item(50.0));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        let bbox = find_box(&root_box, &|x| x.node == Some(b)).unwrap();
        assert!(bbox.dimensions.content.y > abox.dimensions.content.y);
        // b stacks directly below a (a height 30).
        assert!((bbox.dimensions.content.y - (abox.dimensions.content.y + 30.0)).abs() < 0.01);
    }

    #[test]
    fn flex_align_items_center_centers_cross_axis() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let c = doc.append_element(body, "div");
        let a = doc.append_element(c, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            c,
            style::ComputedStyle {
                display: style::Display::Flex,
                display_block: true,
                width: Some(300.0),
                height: Some(100.0),
                align_items: style::AlignItems::Center,
                ..Default::default()
            },
        );
        styles.insert(
            a,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                width: Some(50.0),
                height: Some(20.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let cbox = find_box(&root_box, &|x| x.node == Some(c)).unwrap();
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        // Centered: a.y should be container.y + (100 - 20)/2 = +40.
        let expected_y = cbox.dimensions.content.y + 40.0;
        assert!((abox.dimensions.content.y - expected_y).abs() < 0.01,
            "a.y={} expected {}", abox.dimensions.content.y, expected_y);
    }

    #[test]
    fn absolute_child_offsets_from_positioned_parent_padding_box() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let parent = doc.append_element(body, "div");
        let child = doc.append_element(parent, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            parent,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Relative,
                width: Some(400.0),
                height: Some(300.0),
                padding: style::Edges::all(10.0),
                ..Default::default()
            },
        );
        styles.insert(
            child,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Absolute,
                width: Some(50.0),
                height: Some(50.0),
                top: Some(10.0),
                left: Some(20.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(parent)).unwrap();
        let cbox = find_box(&root_box, &|x| x.node == Some(child)).unwrap();
        let pad = pbox.dimensions.padding_box();
        // Child border-box origin = parent padding-box + (left, top).
        let cb = cbox.dimensions.border_box();
        assert!((cb.x - (pad.x + 20.0)).abs() < 0.01, "cb.x={} pad.x={}", cb.x, pad.x);
        assert!((cb.y - (pad.y + 10.0)).abs() < 0.01, "cb.y={} pad.y={}", cb.y, pad.y);
    }

    #[test]
    fn fixed_child_anchors_to_viewport() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let wrap = doc.append_element(body, "div");
        let child = doc.append_element(wrap, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(wrap, block_style(true));
        styles.insert(
            child,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Fixed,
                width: Some(40.0),
                height: Some(40.0),
                top: Some(10.0),
                left: Some(15.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let cbox = find_box(&root_box, &|x| x.node == Some(child)).unwrap();
        let cb = cbox.dimensions.border_box();
        // Anchored to viewport (0,0): border-box at (15, 10).
        assert!((cb.x - 15.0).abs() < 0.01, "cb.x={}", cb.x);
        assert!((cb.y - 10.0).abs() < 0.01, "cb.y={}", cb.y);
    }

    #[test]
    fn absolute_right_top_shrinks_and_anchors_top_right() {
        // .badge (relative, auto width, height 60, padding 16) contains
        // .corner (absolute, top:6 right:6, padding:4, text "HI").
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let badge = doc.append_element(body, "div");
        let corner = doc.append_element(badge, "div");
        doc.append_child(corner, dom::NodeData::Text("HI".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            badge,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Relative,
                height: Some(60.0),
                padding: style::Edges::all(16.0),
                ..Default::default()
            },
        );
        styles.insert(
            corner,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Absolute,
                top: Some(6.0),
                right: Some(6.0),
                padding: style::Edges::all(4.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(badge)).unwrap();
        let cbox = find_box(&root_box, &|x| x.node == Some(corner)).unwrap();
        let pad = pbox.dimensions.padding_box();
        let bb = cbox.dimensions.border_box();

        // Shrink-to-fit: NOT full width. "HI" = 2*16*0.6 = 19.2 content + 8 padding = 27.2 border box.
        assert!(bb.width < pad.width, "corner border-box width {} should be < parent padding box {}",
            bb.width, pad.width);
        assert!((bb.width - 27.2).abs() < 0.5, "corner border-box width = {}", bb.width);

        // Right edge anchored 6px from the parent's padding-box right edge.
        let right_gap = (pad.x + pad.width) - (bb.x + bb.width);
        assert!((right_gap - 6.0).abs() < 0.01, "right gap = {} (bb.x={} bb.w={})", right_gap, bb.x, bb.width);
        // Top edge anchored 6px from the parent's padding-box top edge.
        assert!((bb.y - (pad.y + 6.0)).abs() < 0.01, "bb.y={} pad.y={}", bb.y, pad.y);
    }

    #[test]
    fn absolute_bottom_anchors_near_parent_bottom() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let parent = doc.append_element(body, "div");
        let child = doc.append_element(parent, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            parent,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Relative,
                width: Some(200.0),
                height: Some(100.0),
                ..Default::default()
            },
        );
        styles.insert(
            child,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Absolute,
                width: Some(30.0),
                height: Some(20.0),
                bottom: Some(8.0),
                left: Some(5.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(parent)).unwrap();
        let cbox = find_box(&root_box, &|x| x.node == Some(child)).unwrap();
        let pad = pbox.dimensions.padding_box();
        let bb = cbox.dimensions.border_box();
        // Border-box bottom edge sits 8px above the parent padding-box bottom edge.
        let bottom_gap = (pad.y + pad.height) - (bb.y + bb.height);
        assert!((bottom_gap - 8.0).abs() < 0.01, "bottom gap = {}", bottom_gap);
        assert!((bb.x - (pad.x + 5.0)).abs() < 0.01, "bb.x={} pad.x={}", bb.x, pad.x);
    }

    #[test]
    fn relative_offsets_without_affecting_siblings() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let a = doc.append_element(body, "div");
        let b = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            a,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                height: Some(30.0),
                position: style::Position::Relative,
                left: Some(25.0),
                top: Some(5.0),
                ..Default::default()
            },
        );
        styles.insert(
            b,
            style::ComputedStyle { display_block: true, height: Some(40.0), ..Default::default() },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        let bbox = find_box(&root_box, &|x| x.node == Some(b)).unwrap();
        // a shifted by (25, 5).
        assert!((abox.dimensions.content.x - 25.0).abs() < 0.01);
        assert!((abox.dimensions.content.y - 5.0).abs() < 0.01);
        // b is unaffected: stacks below a's in-flow position (y=30), not the shifted one.
        assert!((bbox.dimensions.content.y - 30.0).abs() < 0.01, "b.y={}", bbox.dimensions.content.y);
    }

    #[test]
    fn inline_block_sits_inline_with_intrinsic_width() {
        // body > p > [ "ab", inline-block span("XY"), "cd" ]
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let p = doc.append_element(body, "p");
        doc.append_child(p, dom::NodeData::Text("ab".into()));
        let ib = doc.append_element(p, "span");
        doc.append_child(ib, dom::NodeData::Text("XY".into()));
        doc.append_child(p, dom::NodeData::Text("cd".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(p, block_style(true));
        styles.insert(
            ib,
            style::ComputedStyle { display: style::Display::InlineBlock, ..Default::default() },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        // The inline-block becomes an atomic box (node == ib) sitting in the line.
        let ibbox = find_box(&root_box, &|x| x.node == Some(ib)).unwrap();
        // Intrinsic width = "XY" = 2 chars * 16 * 0.6 = 19.2.
        assert!((ibbox.dimensions.content.width - 19.2).abs() < 0.1,
            "ib width = {}", ibbox.dimensions.content.width);
        // It sits to the right of the leading "ab" word (x > content origin 0).
        assert!(ibbox.dimensions.content.x > 0.0);
    }

    #[test]
    fn grid_three_equal_fr_columns_split_width_in_thirds() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let g = doc.append_element(body, "div");
        let a = doc.append_element(g, "div");
        let b = doc.append_element(g, "div");
        let c = doc.append_element(g, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            g,
            style::ComputedStyle {
                display: style::Display::Grid,
                display_block: true,
                width: Some(300.0),
                grid_template_columns: vec![
                    style::TrackSize::Fr(1.0),
                    style::TrackSize::Fr(1.0),
                    style::TrackSize::Fr(1.0),
                ],
                ..Default::default()
            },
        );
        for &id in &[a, b, c] {
            styles.insert(
                id,
                style::ComputedStyle { display_block: true, height: Some(20.0), ..Default::default() },
            );
        }

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        let bbox = find_box(&root_box, &|x| x.node == Some(b)).unwrap();
        let cbox = find_box(&root_box, &|x| x.node == Some(c)).unwrap();
        // Each column 100 wide.
        assert!((abox.dimensions.content.width - 100.0).abs() < 0.01);
        assert!((bbox.dimensions.content.width - 100.0).abs() < 0.01);
        // Columns laid out left-to-right at x = 0, 100, 200 (relative to grid origin).
        let gx = find_box(&root_box, &|x| x.node == Some(g)).unwrap().dimensions.content.x;
        assert!((abox.dimensions.content.x - gx).abs() < 0.01);
        assert!((bbox.dimensions.content.x - (gx + 100.0)).abs() < 0.01);
        assert!((cbox.dimensions.content.x - (gx + 200.0)).abs() < 0.01);
    }

    #[test]
    fn sibling_after_wrapped_paragraph_clears_its_margin_box() {
        // body > p (multi-line wrapped text) , div (sibling). The sibling's content.y must be
        // >= the paragraph block's margin-box bottom (no vertical overlap). This guards the bug
        // where a block with wrapped inline content under-reported its height.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let p = doc.append_element(body, "p");
        doc.append_child(p, dom::NodeData::Text("one two three four five six seven".into()));
        let sib = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(p, block_style(true)); // font_size 16 default
        styles.insert(
            sib,
            style::ComputedStyle { display_block: true, height: Some(10.0), ..Default::default() },
        );

        // Narrow width forces the paragraph to wrap to several lines.
        let root_box = layout_document(&doc, &styles, 60.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(p)).unwrap();
        let sbox = find_box(&root_box, &|x| x.node == Some(sib)).unwrap();

        let lines = count_boxes(pbox, &|x| matches!(x.content, BoxContent::Text(_)));
        assert!(lines > 1, "expected the paragraph to wrap, got {lines} line(s)");
        let p_bottom = pbox.dimensions.margin_box().y + pbox.dimensions.margin_box().height;
        assert!(
            sbox.dimensions.content.y >= p_bottom - 0.01,
            "sibling overlaps paragraph: sib.y={} < p margin-box bottom {}",
            sbox.dimensions.content.y,
            p_bottom
        );
    }

    #[test]
    fn image_box_uses_intrinsic_size_when_no_css() {
        // body > img (no CSS width/height) with intrinsic (100, 50).
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let img = doc.append_element(body, "img");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(img, style::ComputedStyle::default()); // inline by default

        let mut intrinsic = HashMap::new();
        intrinsic.insert(img, (100.0, 50.0));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &intrinsic, None);
        let ibox = find_box(&root_box, &|x| matches!(x.content, BoxContent::Image(_))).unwrap();
        assert_eq!(ibox.dimensions.content.width, 100.0);
        assert_eq!(ibox.dimensions.content.height, 50.0);
        assert_eq!(ibox.node, Some(img));
    }

    #[test]
    fn input_with_value_produces_text_child() {
        // body > input value="hello" → the input box has a Text("hello") child.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let input = doc.append_element(body, "input");
        if let dom::NodeData::Element(e) = &mut doc.get_mut(input).data {
            e.attrs.insert("value".to_string(), "hello".to_string());
        }

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(input, style::ComputedStyle { width: Some(120.0), ..Default::default() });

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let txt = find_box(&root_box, &|x| matches!(&x.content, BoxContent::Text(s) if s == "hello"));
        let txt = txt.expect("input value must render as a Text box");
        // The rendered text traces back to the input element.
        assert_eq!(txt.node, Some(input));
    }

    #[test]
    fn image_box_css_width_preserves_intrinsic_aspect_ratio() {
        // CSS width:200, no height, intrinsic 100x50 (2:1) → height 100.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let img = doc.append_element(body, "img");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            img,
            style::ComputedStyle { width: Some(200.0), ..Default::default() },
        );

        let mut intrinsic = HashMap::new();
        intrinsic.insert(img, (100.0, 50.0));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &intrinsic, None);
        let ibox = find_box(&root_box, &|x| matches!(x.content, BoxContent::Image(_))).unwrap();
        assert_eq!(ibox.dimensions.content.width, 200.0);
        assert!((ibox.dimensions.content.height - 100.0).abs() < 0.01,
            "aspect-preserved height = {}", ibox.dimensions.content.height);
    }

    #[test]
    fn image_box_explicit_both_dimensions() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let img = doc.append_element(body, "img");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            img,
            style::ComputedStyle { width: Some(40.0), height: Some(30.0), ..Default::default() },
        );

        // Intrinsic provided but explicit CSS wins.
        let mut intrinsic = HashMap::new();
        intrinsic.insert(img, (100.0, 50.0));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &intrinsic, None);
        let ibox = find_box(&root_box, &|x| matches!(x.content, BoxContent::Image(_))).unwrap();
        assert_eq!(ibox.dimensions.content.width, 40.0);
        assert_eq!(ibox.dimensions.content.height, 30.0);
    }

    #[test]
    fn block_image_contributes_height_so_sibling_clears_it() {
        // body > img(display:block, 100x50), div(sibling). Sibling must clear the image.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let img = doc.append_element(body, "img");
        let sib = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            img,
            style::ComputedStyle { display: style::Display::Block, display_block: true, ..Default::default() },
        );
        styles.insert(
            sib,
            style::ComputedStyle { display_block: true, height: Some(10.0), ..Default::default() },
        );

        let mut intrinsic = HashMap::new();
        intrinsic.insert(img, (100.0, 50.0));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &intrinsic, None);
        let ibox = find_box(&root_box, &|x| matches!(x.content, BoxContent::Image(_))).unwrap();
        let sbox = find_box(&root_box, &|x| x.node == Some(sib)).unwrap();
        assert_eq!(ibox.dimensions.content.height, 50.0);
        // Sibling stacks below the image's 50px-tall margin box.
        assert!((sbox.dimensions.content.y - 50.0).abs() < 0.01,
            "sibling y = {} (should clear the 50px image)", sbox.dimensions.content.y);
    }

    #[test]
    fn inline_image_advances_the_line() {
        // body > p > [ "ab", img(20x10), "cd" ]: the image is atomic inline, to the right of "ab".
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let p = doc.append_element(body, "p");
        doc.append_child(p, dom::NodeData::Text("ab".into()));
        let img = doc.append_element(p, "img");
        doc.append_child(p, dom::NodeData::Text("cd".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(p, block_style(true));
        styles.insert(img, style::ComputedStyle::default());

        let mut intrinsic = HashMap::new();
        intrinsic.insert(img, (20.0, 10.0));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &intrinsic, None);
        let ibox = find_box(&root_box, &|x| matches!(x.content, BoxContent::Image(_))).unwrap();
        assert_eq!(ibox.dimensions.content.width, 20.0);
        // Sits to the right of the leading "ab" word.
        assert!(ibox.dimensions.content.x > 0.0, "image x = {}", ibox.dimensions.content.x);
    }

    #[test]
    fn image_with_no_size_known_produces_no_box() {
        // No CSS size, no intrinsic entry → nothing to draw, no Image box.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let img = doc.append_element(body, "img");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(img, style::ComputedStyle::default());

        let intrinsic: HashMap<dom::NodeId, (f32, f32)> = HashMap::new();
        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &intrinsic, None);
        assert!(find_box(&root_box, &|x| matches!(x.content, BoxContent::Image(_))).is_none());
    }

    #[test]
    fn flex_column_items_do_not_overlap_and_container_encompasses_them() {
        // A column flex of three items whose heights come from their (wrapped) content, plus a row
        // gap. Items must stack without overlap and the container height must be >= the sum of
        // item heights + gaps.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let c = doc.append_element(body, "div");
        let a = doc.append_element(c, "div");
        let b = doc.append_element(c, "div");
        let d = doc.append_element(c, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            c,
            style::ComputedStyle {
                display: style::Display::Flex,
                display_block: true,
                flex_direction: style::FlexDirection::Column,
                width: Some(200.0),
                row_gap: 8.0,
                ..Default::default()
            },
        );
        // Items have no explicit height; their height is driven by content (one line of text each).
        for &id in &[a, b, d] {
            styles.insert(id, block_style(true));
        }
        doc.append_child(a, dom::NodeData::Text("alpha".into()));
        doc.append_child(b, dom::NodeData::Text("beta".into()));
        doc.append_child(d, dom::NodeData::Text("gamma".into()));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let cbox = find_box(&root_box, &|x| x.node == Some(c)).unwrap();
        let boxes: Vec<_> = [a, b, d]
            .iter()
            .map(|&id| find_box(&root_box, &|x| x.node == Some(id)).unwrap())
            .collect();

        // Each item starts at or below the previous item's margin-box bottom.
        let mut sum_h = 0.0f32;
        for i in 0..boxes.len() {
            let mb = boxes[i].dimensions.margin_box();
            sum_h += mb.height;
            if i > 0 {
                let prev = boxes[i - 1].dimensions.margin_box();
                assert!(
                    boxes[i].dimensions.content.y >= prev.y + prev.height - 0.01,
                    "flex column item {i} overlaps the previous: y={} prev-bottom={}",
                    boxes[i].dimensions.content.y,
                    prev.y + prev.height
                );
            }
        }
        // Each item should actually have a non-zero height (the (792,275) zero-height bug).
        for (i, bx) in boxes.iter().enumerate() {
            assert!(bx.dimensions.content.height > 0.0, "item {i} has zero height");
        }
        // Container height >= sum of item heights + gaps between them.
        let expected_min = sum_h + 8.0 * 2.0;
        assert!(
            cbox.dimensions.content.height >= expected_min - 0.01,
            "container height {} < items+gaps {}",
            cbox.dimensions.content.height,
            expected_min
        );
    }

    /// Set an attribute on an element node (test helper).
    fn set_attr(doc: &mut dom::Document, id: dom::NodeId, name: &str, value: &str) {
        if let dom::NodeData::Element(e) = &mut doc.get_mut(id).data {
            e.attrs.insert(name.to_string(), value.to_string());
        }
    }

    /// Does any text run in the subtree contain `needle`?
    fn has_text(b: &LayoutBox, needle: &str) -> bool {
        find_box(b, &|x| matches!(&x.content, BoxContent::Text(s) if s.contains(needle))).is_some()
    }

    #[test]
    fn checked_checkbox_renders_checked_indicator() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let input = doc.append_element(body, "input");
        set_attr(&mut doc, input, "type", "checkbox");
        set_attr(&mut doc, input, "checked", "");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(input, style::ComputedStyle::default());

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let ibox = find_box(&root_box, &|x| x.node == Some(input)).unwrap();
        // The checkbox is a drawn Widget; checked state is carried on the Widget content.
        assert!(
            matches!(ibox.content, BoxContent::Widget(WidgetKind::Checkbox { checked: true })),
            "expected a checked Checkbox widget, got {:?}",
            ibox.content
        );
        // It has a non-zero box so it paints (and hit-tests).
        assert!(ibox.dimensions.content.width > 0.0 && ibox.dimensions.content.height > 0.0);
    }

    #[test]
    fn unchecked_checkbox_renders_empty_indicator() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let input = doc.append_element(body, "input");
        set_attr(&mut doc, input, "type", "checkbox");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(input, style::ComputedStyle::default());

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let ibox = find_box(&root_box, &|x| x.node == Some(input)).unwrap();
        assert!(
            matches!(ibox.content, BoxContent::Widget(WidgetKind::Checkbox { checked: false })),
            "expected an unchecked Checkbox widget, got {:?}",
            ibox.content
        );
    }

    /// Build a `<select>` with the given options. Each option is `(value_attr, text, selected)`;
    /// `value_attr = None` means no `value` attribute. Returns `(doc, select_id, body)`.
    fn build_select(
        options: &[(Option<&str>, &str, bool)],
        select_value: Option<&str>,
    ) -> (dom::Document, dom::NodeId, dom::NodeId) {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let select = doc.append_element(body, "select");
        if let Some(v) = select_value {
            set_attr(&mut doc, select, "value", v);
        }
        for &(val, text, selected) in options {
            let opt = doc.append_element(select, "option");
            if let Some(v) = val {
                set_attr(&mut doc, opt, "value", v);
            }
            if selected {
                set_attr(&mut doc, opt, "selected", "");
            }
            doc.append_child(opt, dom::NodeData::Text(text.to_string()));
        }
        (doc, select, body)
    }

    fn layout_select(
        doc: &dom::Document,
        select: dom::NodeId,
        body: dom::NodeId,
    ) -> LayoutBox {
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(select, style::ComputedStyle::default());
        // Style the options + their text so they would lay out if (wrongly) recursed into.
        for &child in &doc.get(select).children {
            styles.insert(child, style::ComputedStyle::default());
        }
        layout_document(doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None)
    }

    #[test]
    fn select_renders_selected_option_as_dropdown() {
        // Three options, the 2nd is `selected`.
        let (doc, select, body) = build_select(
            &[(None, "First", false), (None, "Second", true), (None, "Third", false)],
            None,
        );
        let root_box = layout_select(&doc, select, body);
        let sbox = find_box(&root_box, &|x| x.node == Some(select)).unwrap();
        // Shows the selected label and the dropdown arrow.
        assert!(has_text(sbox, "Second"), "expected selected option label");
        assert!(has_text(sbox, "\u{25BE}"), "expected dropdown arrow ▾");
        // Does NOT show the other options.
        assert!(!has_text(sbox, "First"), "unselected option leaked");
        assert!(!has_text(sbox, "Third"), "unselected option leaked");
    }

    #[test]
    fn select_value_attr_selects_matching_option() {
        // No `selected`; `value` attr matches the 3rd option's value.
        let (doc, select, body) = build_select(
            &[
                (Some("a"), "Apple", false),
                (Some("b"), "Banana", false),
                (Some("c"), "Cherry", false),
            ],
            Some("c"),
        );
        let root_box = layout_select(&doc, select, body);
        let sbox = find_box(&root_box, &|x| x.node == Some(select)).unwrap();
        assert!(has_text(sbox, "Cherry"), "value=c should select the Cherry option");
        assert!(!has_text(sbox, "Apple"));
        assert!(!has_text(sbox, "Banana"));
    }

    #[test]
    fn select_defaults_to_first_option() {
        // No `selected`, no `value` → first option shows.
        let (doc, select, body) = build_select(
            &[(None, "One", false), (None, "Two", false), (None, "Three", false)],
            None,
        );
        let root_box = layout_select(&doc, select, body);
        let sbox = find_box(&root_box, &|x| x.node == Some(select)).unwrap();
        assert!(has_text(sbox, "One"), "first option should show by default");
        assert!(!has_text(sbox, "Two"));
        assert!(!has_text(sbox, "Three"));
    }

    #[test]
    fn select_options_are_not_separate_inline_boxes() {
        // The <option> DOM subtree must be suppressed: no Text box should carry an option node id,
        // and the unselected options' text must not appear anywhere in the layout tree.
        let (doc, select, body) = build_select(
            &[(None, "Alpha", true), (None, "Beta", false), (None, "Gamma", false)],
            None,
        );
        let option_ids: Vec<dom::NodeId> = doc.get(select).children.clone();
        let root_box = layout_select(&doc, select, body);
        // No box anywhere is owned by an <option> element/text node.
        for opt in option_ids {
            assert!(
                find_box(&root_box, &|x| x.node == Some(opt)).is_none(),
                "an <option> produced its own box (should be suppressed)"
            );
        }
        assert!(!has_text(&root_box, "Beta"), "unselected option text leaked into layout");
        assert!(!has_text(&root_box, "Gamma"), "unselected option text leaked into layout");
    }

    #[test]
    fn focused_text_input_shows_caret() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let input = doc.append_element(body, "input");
        set_attr(&mut doc, input, "type", "text");
        set_attr(&mut doc, input, "value", "hi");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(input, style::ComputedStyle::default());

        // Focused: the value text is "hi" (no pipe glyph) plus a separate caret bar box. The caret
        // is laid out as a sibling of the value run (both owned by the input), so search the tree.
        let root_box =
            layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), Some(input));
        assert!(has_text(&root_box, "hi"), "focused input should still show its value");
        assert!(!has_text(&root_box, "|"), "caret must be a bar, not a pipe glyph");
        let caret = find_box(&root_box, &|x| matches!(x.content, BoxContent::Caret))
            .expect("focused input should have a caret bar box");
        assert_eq!(caret.node, Some(input), "caret belongs to the focused input");
        assert!(
            caret.dimensions.content.width > 0.0 && caret.dimensions.content.height > 0.0,
            "caret bar has nonzero size"
        );

        // Not focused: no caret box, no pipe.
        let root_box2 = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        assert!(has_text(&root_box2, "hi"), "unfocused input still shows its value");
        assert!(!has_text(&root_box2, "|"), "unfocused input must not show a caret");
        assert_eq!(
            count_boxes(&root_box2, &|x| matches!(x.content, BoxContent::Caret)),
            0,
            "unfocused input must not have a caret bar box"
        );
    }

    #[test]
    fn empty_focused_input_shows_caret_not_placeholder() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let input = doc.append_element(body, "input");
        set_attr(&mut doc, input, "type", "text");
        set_attr(&mut doc, input, "placeholder", "Search");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(input, style::ComputedStyle::default());

        // Focused + empty: a caret bar, and the placeholder is hidden (as in real browsers).
        let root_box =
            layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), Some(input));
        assert!(
            count_boxes(&root_box, &|x| matches!(x.content, BoxContent::Caret)) >= 1,
            "empty focused input should show a caret bar"
        );
        assert!(
            !has_text(&root_box, "Search"),
            "placeholder must be hidden while a focused field is being edited"
        );

        // Unfocused + empty: the placeholder shows and there's no caret.
        let root_box2 = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let ibox2 = find_box(&root_box2, &|x| x.node == Some(input)).unwrap();
        assert!(has_text(ibox2, "Search"), "unfocused empty input keeps its placeholder");
        assert_eq!(
            count_boxes(ibox2, &|x| matches!(x.content, BoxContent::Caret)),
            0,
            "unfocused input has no caret"
        );
    }

    // ----- generated content (::before / ::after) -----

    /// A pseudo computed style carrying a content string (inline by default).
    fn pseudo_style(content: &str) -> style::ComputedStyle {
        style::ComputedStyle { content: Some(content.to_string()), ..Default::default() }
    }

    /// The first child of the box for `node` whose text equals `s` and its index among children.
    fn child_text_at<'a>(b: &'a LayoutBox) -> Vec<&'a str> {
        b.children
            .iter()
            .filter_map(|c| match &c.content {
                BoxContent::Text(t) => Some(t.as_str()),
                _ => c.children.first().and_then(|cc| match &cc.content {
                    BoxContent::Text(t) => Some(t.as_str()),
                    _ => None,
                }),
            })
            .collect()
    }

    #[test]
    fn before_text_precedes_real_text() {
        // <div class=x>hi</div> with ::before content "→".
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let div = doc.append_element(body, "div");
        doc.append_child(div, dom::NodeData::Text("hi".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            div,
            style::ComputedStyle {
                display_block: true,
                before: Some(Box::new(pseudo_style("→"))),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let dbox = find_box(&root_box, &|x| x.node == Some(div)).unwrap();
        // The ::before "→" text must appear before "hi" in document order.
        let texts = collect_text_boxes(dbox)
            .iter()
            .filter_map(|b| match &b.content {
                BoxContent::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["→".to_string(), "hi".to_string()]);
    }

    #[test]
    fn after_text_follows_real_text() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let div = doc.append_element(body, "div");
        doc.append_child(div, dom::NodeData::Text("hi".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            div,
            style::ComputedStyle {
                display_block: true,
                after: Some(Box::new(pseudo_style("world"))),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let dbox = find_box(&root_box, &|x| x.node == Some(div)).unwrap();
        let texts = collect_text_boxes(dbox)
            .iter()
            .filter_map(|b| match &b.content {
                BoxContent::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        // "hi" and "world" are separate inline words on the same line; ::after comes last.
        assert!(texts.iter().any(|t| t.contains("hi")));
        let joined = texts.join(" ");
        let hi_pos = joined.find("hi").unwrap();
        let world_pos = joined.find("world").unwrap();
        assert!(hi_pos < world_pos, "::after text must follow the element's own text");
    }

    #[test]
    fn empty_content_emits_no_text_box() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let div = doc.append_element(body, "div");
        doc.append_child(div, dom::NodeData::Text("hi".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            div,
            style::ComputedStyle {
                display_block: true,
                after: Some(Box::new(pseudo_style(""))),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let dbox = find_box(&root_box, &|x| x.node == Some(div)).unwrap();
        let texts: Vec<_> = collect_text_boxes(dbox)
            .iter()
            .filter_map(|b| match &b.content {
                BoxContent::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        // Only the real "hi" text — the empty ::after contributes a box but no text child.
        assert_eq!(texts, vec!["hi".to_string()]);
        let _ = child_text_at(dbox);
    }

    #[test]
    fn inline_pseudo_text_carries_its_own_color() {
        // An inline ::before is flattened into text runs; the run carries the pseudo's color.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let div = doc.append_element(body, "div");

        let mut pseudo = pseudo_style("x");
        pseudo.color = (255, 0, 0);

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            div,
            style::ComputedStyle {
                display_block: true,
                color: (0, 0, 255),
                before: Some(Box::new(pseudo)),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let dbox = find_box(&root_box, &|x| x.node == Some(div)).unwrap();
        let tb = collect_text_boxes(dbox)
            .into_iter()
            .find(|b| matches!(&b.content, BoxContent::Text(t) if t == "x"))
            .expect("::before text box");
        assert_eq!(tb.style.color, (255, 0, 0)); // pseudo red, distinct from element blue
    }

    #[test]
    fn block_pseudo_box_carries_background() {
        // A `display: block` ::before keeps its own box (not flattened), so its background applies.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let div = doc.append_element(body, "div");

        let mut pseudo = pseudo_style("x");
        pseudo.display = style::Display::Block;
        pseudo.display_block = true;
        pseudo.background_color = Some((0, 255, 0));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            div,
            style::ComputedStyle {
                display_block: true,
                before: Some(Box::new(pseudo)),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let dbox = find_box(&root_box, &|x| x.node == Some(div)).unwrap();
        let pseudo_box = dbox
            .children
            .iter()
            .find(|c| c.node.is_none() && matches!(c.content, BoxContent::Block))
            .expect("anonymous ::before block box");
        assert_eq!(pseudo_box.style.background_color, Some((0, 255, 0)));
    }

    // ------------------------------------------------------------------------------------------
    // Table layout
    // ------------------------------------------------------------------------------------------

    /// A computed style with a given `display` value (everything else default).
    fn disp(d: style::Display) -> style::ComputedStyle {
        style::ComputedStyle { display: d, ..Default::default() }
    }

    /// Build a `tr`-of-cells row under `parent`, returning the cell node ids. `cell_tag` is `td`/`th`.
    /// Each cell gets a single text node. `styles` is populated with table-* display values.
    fn build_row(
        doc: &mut dom::Document,
        styles: &mut HashMap<dom::NodeId, style::ComputedStyle>,
        parent: dom::NodeId,
        cell_tag: &str,
        texts: &[&str],
    ) -> Vec<dom::NodeId> {
        let tr = doc.append_element(parent, "tr");
        styles.insert(tr, disp(style::Display::TableRow));
        let mut cells = Vec::new();
        for t in texts {
            let cell = doc.append_element(tr, cell_tag);
            let mut cs = disp(style::Display::TableCell);
            if cell_tag == "th" {
                cs.bold = true;
                cs.text_align = style::TextAlign::Center;
            }
            styles.insert(cell, cs);
            doc.append_child(cell, dom::NodeData::Text((*t).into()));
            cells.push(cell);
        }
        cells
    }

    #[test]
    fn table_3x3_columns_align_rows_share_y() {
        // A 3x3 table: cells in the same column share x + width; cells in the same row share y.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let table = doc.append_element(body, "table");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(table, disp(style::Display::Table));

        let mut rows: Vec<Vec<dom::NodeId>> = Vec::new();
        rows.push(build_row(&mut doc, &mut styles, table, "td", &["aa", "bbbb", "c"]));
        rows.push(build_row(&mut doc, &mut styles, table, "td", &["dddddd", "e", "ff"]));
        rows.push(build_row(&mut doc, &mut styles, table, "td", &["g", "hh", "iii"]));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);

        let cell_rect = |n: dom::NodeId| {
            find_box(&root_box, &|x| x.node == Some(n) && matches!(x.content, BoxContent::Block))
                .unwrap()
                .dimensions
                .border_box()
        };

        // Columns: x + width match down each column.
        for col in 0..3 {
            let r0 = cell_rect(rows[0][col]);
            for row in 1..3 {
                let r = cell_rect(rows[row][col]);
                assert!((r.x - r0.x).abs() < 0.01, "col {col} x mismatch: {} vs {}", r.x, r0.x);
                assert!((r.width - r0.width).abs() < 0.01, "col {col} width mismatch");
            }
        }
        // Rows: y matches across each row.
        for row in 0..3 {
            let r0 = cell_rect(rows[row][0]);
            for col in 1..3 {
                let r = cell_rect(rows[row][col]);
                assert!((r.y - r0.y).abs() < 0.01, "row {row} y mismatch: {} vs {}", r.y, r0.y);
            }
        }
        // Column 0's width is driven by its widest cell ("dddddd").
        let c00 = cell_rect(rows[0][0]);
        let c01 = cell_rect(rows[0][1]);
        assert!(c00.x < c01.x, "column 0 sits left of column 1");
    }

    #[test]
    fn table_thead_tbody_renders_all_cells() {
        // A table whose rows live inside <thead>/<tbody> must render every cell (regression: row
        // groups used to be inline and their rows vanished).
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let table = doc.append_element(body, "table");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(table, disp(style::Display::Table));

        let thead = doc.append_element(table, "thead");
        styles.insert(thead, disp(style::Display::TableHeaderGroup));
        let h = build_row(&mut doc, &mut styles, thead, "th", &["H1", "H2"]);

        let tbody = doc.append_element(table, "tbody");
        styles.insert(tbody, disp(style::Display::TableRowGroup));
        let r1 = build_row(&mut doc, &mut styles, tbody, "td", &["a", "b"]);
        let r2 = build_row(&mut doc, &mut styles, tbody, "td", &["c", "d"]);

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);

        for n in h.iter().chain(r1.iter()).chain(r2.iter()) {
            assert!(
                find_box(&root_box, &|x| x.node == Some(*n)).is_some(),
                "cell {n:?} should produce a box"
            );
        }
        // Header sits above the body rows.
        let hy = find_box(&root_box, &|x| x.node == Some(h[0])).unwrap().dimensions.content.y;
        let by = find_box(&root_box, &|x| x.node == Some(r1[0])).unwrap().dimensions.content.y;
        assert!(hy < by, "thead row ({hy}) above tbody row ({by})");
    }

    #[test]
    fn table_th_is_bold_and_centered() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let table = doc.append_element(body, "table");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(table, disp(style::Display::Table));
        let h = build_row(&mut doc, &mut styles, table, "th", &["Header"]);
        // Give the cell an explicit width wider than its text so centering has room to show.
        if let Some(cs) = styles.get_mut(&h[0]) {
            cs.width = Some(200.0);
        }

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let cell = find_box(&root_box, &|x| x.node == Some(h[0]) && matches!(x.content, BoxContent::Block)).unwrap();
        // The cell's text run is bold.
        let text = find_box(cell, &|x| matches!(x.content, BoxContent::Text(_))).unwrap();
        assert!(text.style.bold, "th text should be bold");
        // Centered: the text run is horizontally centered within the cell content box.
        let cell_box = cell.dimensions.content;
        let tr = text.dimensions.content;
        let left_gap = tr.x - cell_box.x;
        let right_gap = (cell_box.x + cell_box.width) - (tr.x + tr.width);
        assert!(left_gap > 0.5, "expected left padding from centering, got {left_gap}");
        assert!((left_gap - right_gap).abs() < 1.0, "text not centered: L={left_gap} R={right_gap}");
    }

    #[test]
    fn table_colspan_spans_two_columns() {
        // Row 1: two cells (define two columns). Row 2: one cell with colspan=2 spanning both.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let table = doc.append_element(body, "table");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(table, disp(style::Display::Table));

        let r1 = build_row(&mut doc, &mut styles, table, "td", &["aaaa", "bbbb"]);

        let tr2 = doc.append_element(table, "tr");
        styles.insert(tr2, disp(style::Display::TableRow));
        let wide = doc.append_element(tr2, "td");
        let mut wcs = disp(style::Display::TableCell);
        wcs.bold = false;
        styles.insert(wide, wcs);
        if let dom::NodeData::Element(el) = &mut doc.get_mut(wide).data {
            el.attrs.insert("colspan".into(), "2".into());
        }
        doc.append_child(wide, dom::NodeData::Text("spanning".into()));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);

        let c0 = find_box(&root_box, &|x| x.node == Some(r1[0]) && matches!(x.content, BoxContent::Block)).unwrap().dimensions.border_box();
        let c1 = find_box(&root_box, &|x| x.node == Some(r1[1]) && matches!(x.content, BoxContent::Block)).unwrap().dimensions.border_box();
        let span = find_box(&root_box, &|x| x.node == Some(wide) && matches!(x.content, BoxContent::Block)).unwrap().dimensions.border_box();

        // The spanning cell's border box covers both columns: from col0.x to col1's right edge.
        assert!((span.x - c0.x).abs() < 0.5, "spanning cell starts at col0 x");
        let two_col_w = c0.width + c1.width;
        assert!((span.width - two_col_w).abs() < 0.5, "colspan=2 width {} != {}", span.width, two_col_w);
    }

    #[test]
    fn table_caption_sits_above_first_row() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let table = doc.append_element(body, "table");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(table, disp(style::Display::Table));

        let caption = doc.append_element(table, "caption");
        styles.insert(caption, disp(style::Display::TableCaption));
        doc.append_child(caption, dom::NodeData::Text("My Caption".into()));

        let r1 = build_row(&mut doc, &mut styles, table, "td", &["a", "b"]);

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let cap_box = find_box(&root_box, &|x| x.node == Some(caption)).unwrap();
        let cell_box = find_box(&root_box, &|x| x.node == Some(r1[0]) && matches!(x.content, BoxContent::Block)).unwrap();
        assert!(
            cap_box.dimensions.content.y < cell_box.dimensions.content.y,
            "caption ({}) should sit above the first cell ({})",
            cap_box.dimensions.content.y,
            cell_box.dimensions.content.y
        );
    }

    #[test]
    fn table_cell_content_wraps_within_column_width() {
        // A narrow fixed-width cell forces its long text to wrap onto multiple lines, making the
        // cell (and its row) taller than a single line.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let table = doc.append_element(body, "table");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        let mut tcs = disp(style::Display::Table);
        tcs.width = Some(60.0); // narrow table → narrow column → wrapping
        styles.insert(table, tcs);

        let tr = doc.append_element(table, "tr");
        styles.insert(tr, disp(style::Display::TableRow));
        let cell = doc.append_element(tr, "td");
        styles.insert(cell, disp(style::Display::TableCell));
        doc.append_child(cell, dom::NodeData::Text("one two three four five six".into()));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let cell_box = find_box(&root_box, &|x| x.node == Some(cell) && matches!(x.content, BoxContent::Block)).unwrap();
        // More than one line box of text => wrapped.
        let lines = collect_text_boxes(cell_box);
        assert!(lines.len() > 1, "cell content should wrap to multiple lines, got {}", lines.len());
        // The cell content width should not exceed the (narrow) column.
        assert!(cell_box.dimensions.content.width <= 60.0 + 0.5, "cell wider than column");
    }
}

