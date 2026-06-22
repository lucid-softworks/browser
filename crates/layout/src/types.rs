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
    /// `border-collapse` (inherited from the table). On a collapsed table the painter draws cell
    /// borders as single shared 1px lines (top+left of each cell, plus the table's right/bottom
    /// rim) instead of per-box 4-edge fills, so adjacent cells don't double up. Read by the painter.
    pub border_collapse: style::BorderCollapse,
    /// True for a `display: table-cell` box. Lets the painter apply the collapsed-borders single-line
    /// rule to cells (top+left lines only) while the table box keeps its full outer border frame.
    pub is_table_cell: bool,
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
    /// `visibility: visible`. `false` for `hidden`/`collapse`: the box keeps its layout box but the
    /// painter skips its own content (background, border, text, image). Because `visibility`
    /// inherits, descendants are `false` too unless one opts back in with `visibility: visible` —
    /// so the painter still recurses into children rather than culling the subtree.
    pub visible: bool,
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
    /// A `mask-image` source (the icon technique), if any. When set, the painter composites the
    /// box's background through the mask's opaque (alpha) pixels. `None` = unmasked.
    pub mask_image: Option<style::MaskImage>,
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
            border_collapse: style::BorderCollapse::Separate,
            is_table_cell: false,
            underline: false,
            line_through: false,
            overline: false,
            vertical_align: style::VerticalAlign::Baseline,
            white_space: style::WhiteSpace::Normal,
            opacity: 1.0,
            visible: true,
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
        self.extras
            .as_deref()
            .map(|e| e.border_radius)
            .unwrap_or(0.0)
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
    /// The CSSOM *used* values of the inset properties `[top, right, bottom, left]` (px), filled in
    /// for positioned boxes (relative / absolute / fixed / sticky) once their containing block is
    /// known. `getComputedStyle(el).top` etc. return these when the element has a box. `None` for
    /// non-positioned / box-less elements (where the computed value is reported instead). Each side
    /// is the offset from the containing block edge to the box's margin-box edge — including the
    /// static-position fallback when both opposite insets are `auto`.
    pub used_insets: Option<[f32; 4]>,
    /// Used (resolved) margins `[top, right, bottom, left]` in px once `auto` is resolved, so
    /// `getComputedStyle` can report the used value of a `margin: auto` block. `None` until laid out.
    pub used_margins: Option<[f32; 4]>,
}

impl LayoutBox {
    pub fn new(content: BoxContent, style: PaintStyle, node: Option<dom::NodeId>) -> Self {
        LayoutBox {
            dimensions: Dimensions::default(),
            content,
            node,
            style,
            children: Vec::new(),
            used_insets: None,
            used_margins: None,
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
