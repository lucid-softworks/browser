//! Selector matching + the cascade.
//!
//! [`cascade`] walks a [`dom::Document`], matches a built-in user-agent stylesheet plus the
//! author `<style>` sheets and inline `style="…"` attributes against each element, resolves
//! the winning declarations by origin + specificity + source order, applies inheritance, and
//! returns a [`ComputedStyle`] per element [`dom::NodeId`].
//!
//! Supported selectors are *simple*: type/tag (`p`), class (`.x`), id (`#id`), the universal
//! selector (`*`), and grouped comma lists. A single compound like `p.note` (a tag plus one
//! class/id) is also handled. Descendant combinators (`div p`) are NOT supported.

use std::collections::HashMap;

/// The four sides of a box (margin / border / padding thicknesses, or content insets), in px.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Edges {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Edges {
    /// All four sides set to the same value.
    pub fn all(v: f32) -> Self {
        Edges { top: v, right: v, bottom: v, left: v }
    }
}

/// The `display` mode of an element. Drives which layout algorithm lays out its children
/// and how the element itself participates in its parent's flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    None,
    Block,
    Inline,
    InlineBlock,
    Flex,
    InlineFlex,
    Grid,
    InlineGrid,
    /// `display: table` — establishes a table formatting context (a grid of rows/cells).
    Table,
    /// `display: table-row` (`<tr>`).
    TableRow,
    /// `display: table-cell` (`<td>`/`<th>`).
    TableCell,
    /// `display: table-row-group` (`<tbody>`).
    TableRowGroup,
    /// `display: table-header-group` (`<thead>`).
    TableHeaderGroup,
    /// `display: table-footer-group` (`<tfoot>`).
    TableFooterGroup,
    /// `display: table-caption` (`<caption>`).
    TableCaption,
    /// `display: table-column` (`<col>`).
    TableColumn,
    /// `display: table-column-group` (`<colgroup>`).
    TableColumnGroup,
}

/// CSS `position`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Static,
    Relative,
    Absolute,
    Fixed,
    Sticky,
}

/// The *specified* value of an inset longhand (`top`/`right`/`bottom`/`left`), retained so the
/// CSSOM "resolved value" algorithm can be applied at `getComputedStyle` time. Absolute lengths
/// (incl. `em`/`rem`) are absolutized to px during the cascade; percentages and `calc()` mixing a
/// percentage are kept symbolic because their basis (the containing block) isn't known until then.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InsetValue {
    /// `auto` (or unset).
    Auto,
    /// An absolute length, already resolved to CSS px.
    Length(f32),
    /// A percentage, stored as the raw number (e.g. `10%` → `10.0`).
    Percent(f32),
    /// `calc()` mixing a percentage and a length: `pct`% of the basis plus `px` px.
    Calc { pct: f32, px: f32 },
}

impl InsetValue {
    /// Serialize the *specified* value as CSSOM would for a box-less / percentage-preserving case.
    pub fn serialize_specified(&self) -> String {
        match self {
            InsetValue::Auto => "auto".to_string(),
            InsetValue::Length(v) => px(*v),
            InsetValue::Percent(p) => format!("{}%", num(*p)),
            // calc() with a percentage serializes in canonical form.
            InsetValue::Calc { pct, px: l } => {
                if *l == 0.0 {
                    format!("calc({}%)", num(*pct))
                } else if *l > 0.0 {
                    format!("calc({}% + {}px)", num(*pct), num(*l))
                } else {
                    format!("calc({}% - {}px)", num(*pct), num(-*l))
                }
            }
        }
    }

    /// Resolve to a used px length given the percentage `basis` (the containing-block extent on the
    /// relevant axis). `Auto` yields `None`.
    pub fn resolve_px(&self, basis: f32) -> Option<f32> {
        match self {
            InsetValue::Auto => None,
            InsetValue::Length(v) => Some(*v),
            InsetValue::Percent(p) => Some(p / 100.0 * basis),
            InsetValue::Calc { pct, px: l } => Some(pct / 100.0 * basis + l),
        }
    }
}

/// Flex container main-axis direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexDirection {
    Row,
    RowReverse,
    Column,
    ColumnReverse,
}

/// Flex container wrapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexWrap {
    NoWrap,
    Wrap,
    WrapReverse,
}

/// Main-axis distribution of flex items / cross-axis distribution of flex lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JustifyContent {
    FlexStart,
    FlexEnd,
    Center,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

/// Cross-axis alignment of flex items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignItems {
    Stretch,
    FlexStart,
    FlexEnd,
    Center,
    Baseline,
}

/// Per-item cross-axis alignment override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignSelf {
    Auto,
    Stretch,
    FlexStart,
    FlexEnd,
    Center,
    Baseline,
}

/// A grid track size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrackSize {
    Px(f32),
    Fr(f32),
    Pct(f32),
    Auto,
}

/// A grid line placement: `(start_line, GridSpan)`. Lines are 1-based as in CSS.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridPlacement {
    /// 1-based start line (`None` = auto-place).
    pub start: Option<i32>,
    /// End placement.
    pub end: GridEnd,
}

/// The end side of a grid placement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GridEnd {
    /// Auto (single cell unless a span widens it).
    Auto,
    /// An explicit 1-based end line.
    Line(i32),
    /// Span N tracks from the start.
    Span(i32),
}

impl Default for GridPlacement {
    fn default() -> Self {
        GridPlacement { start: None, end: GridEnd::Auto }
    }
}

/// The computed style for a single element.
#[derive(Debug, Clone, PartialEq)]
pub struct ComputedStyle {
    /// Text color (r, g, b).
    pub color: (u8, u8, u8),
    /// Background color, if any (r, g, b). `None` means transparent.
    pub background_color: Option<(u8, u8, u8)>,
    /// Font size in pixels.
    pub font_size: f32,
    /// The specified `font-family` list, serialized to CSSOM canonical form (quoting normalized).
    /// `None` = the UA/initial default (reported as the empty string). Inherited.
    pub font_family: Option<String>,
    pub bold: bool,
    pub italic: bool,
    pub text_align: TextAlign,
    /// `display: none` hides the element and its subtree. Derived from [`display`](Self::display).
    pub display_none: bool,
    /// Whether this element participates in block flow (`display: block`) vs inline.
    /// Derived from [`display`](Self::display); kept for existing readers.
    pub display_block: bool,
    /// The full display mode. Drives layout dispatch.
    pub display: Display,
    /// CSS `position`. Not inherited.
    pub position: Position,
    /// Inset `top` in px (`None` = auto). Percentages unsupported (stored as None).
    pub top: Option<f32>,
    pub right: Option<f32>,
    pub bottom: Option<f32>,
    pub left: Option<f32>,
    /// *Specified* inset values (px-absolutized lengths, plus symbolic percentages/`calc()` and
    /// `auto`), retained for the CSSOM resolved-value algorithm. Mirror the order above.
    pub top_spec: InsetValue,
    pub right_spec: InsetValue,
    pub bottom_spec: InsetValue,
    pub left_spec: InsetValue,
    /// Stacking `z-index` (`None` = auto). Parsed but not yet used for paint ordering.
    pub z_index: Option<i32>,
    /// Explicit content `width` in px (`None` = auto). Percentages are ignored (None).
    pub width: Option<f32>,
    /// Explicit content `height` in px (`None` = auto).
    pub height: Option<f32>,
    /// `min-width` constraint (`None` = 0/unset). Resolved against the containing block in layout.
    pub min_width: Option<SizeConstraint>,
    /// `max-width` constraint (`None`/`none` = no maximum).
    pub max_width: Option<SizeConstraint>,
    /// `min-height` constraint (`None` = 0/unset).
    pub min_height: Option<SizeConstraint>,
    /// `max-height` constraint (`None`/`none` = no maximum).
    pub max_height: Option<SizeConstraint>,
    /// Margin thicknesses (px). Not inherited.
    pub margin: Edges,
    /// Padding thicknesses (px). Not inherited.
    pub padding: Edges,
    /// Border *widths* (px). Not inherited.
    pub border: Edges,
    /// Border color (r, g, b).
    pub border_color: (u8, u8, u8),
    /// Whether `overflow` (x or y) is anything other than `visible` (i.e. `hidden`/`scroll`/`auto`/
    /// `clip`). Such a box is a *scroll container* / scrollport — the containing block against which a
    /// `position: sticky` descendant's inset percentages resolve (CSSOM resolved value). Not inherited.
    pub overflow_scrollport: bool,

    // --- Table properties ---
    /// `border-collapse` (`separate` default | `collapse`). On a `display: table`, `Collapse`
    /// switches the layout to the collapsed-borders model (cells flush, single shared edge lines).
    /// Inherits (per CSS — it's set on the table and read by its cells in layout/paint).
    pub border_collapse: BorderCollapse,
    /// `border-spacing` in px (the gap between adjacent cells in the separated-borders model).
    /// Inherits. Ignored when `border_collapse == Collapse`.
    pub border_spacing: f32,

    // --- Flex container properties ---
    pub flex_direction: FlexDirection,
    pub flex_wrap: FlexWrap,
    pub justify_content: JustifyContent,
    pub align_items: AlignItems,
    /// Cross-axis distribution of flex lines (multi-line). `None` = default (stretch-ish).
    pub align_content: Option<JustifyContent>,

    // --- Flex item properties ---
    pub flex_grow: f32,
    pub flex_shrink: f32,
    /// Flex basis in px (`None` = auto).
    pub flex_basis: Option<f32>,
    pub align_self: AlignSelf,
    pub order: i32,

    // --- Gaps (flex & grid) ---
    pub row_gap: f32,
    pub column_gap: f32,

    // --- Grid container properties ---
    pub grid_template_columns: Vec<TrackSize>,
    pub grid_template_rows: Vec<TrackSize>,

    // --- Grid item placement ---
    pub grid_column: Option<GridPlacement>,
    pub grid_row: Option<GridPlacement>,

    // --- Text / typography extras ---
    /// Resolved `line-height` in px (`None` = use the font metric default). Inherits.
    pub line_height: Option<f32>,
    /// `text-transform`. Inherits.
    pub text_transform: TextTransform,
    /// `letter-spacing` in px added per character (0 = normal). Inherits.
    pub letter_spacing: f32,
    /// `white-space` processing mode (collapse vs preserve spaces/newlines). Inherits.
    pub white_space: WhiteSpace,
    /// `list-style-type` marker style for `display: list-item` boxes (`ul`/`ol`/`li`). Inherits.
    pub list_style_type: ListStyleType,

    // --- Paint extras ---
    /// `text-decoration` underline flag. Inherits.
    pub underline: bool,
    /// `text-decoration` line-through flag. Inherits.
    pub line_through: bool,
    /// `text-decoration` overline flag (line above the text). Inherits.
    pub overline: bool,
    /// `vertical-align` for inline-level boxes. Drives `sub`/`super` baseline shifts. Not inherited.
    pub vertical_align: VerticalAlign,
    /// `opacity` in 0.0..=1.0 (1.0 = fully opaque). Not inherited (composited per-box).
    pub opacity: f32,
    /// Uniform `border-radius` in px (0 = square corners). Not inherited.
    pub border_radius: f32,
    /// A `background-image` gradient (linear/radial), if any. `None` = no gradient. Painted as the
    /// box's background fill (over any solid `background-color`). Not inherited.
    pub background_gradient: Option<Gradient>,
    /// `box-shadow` layers (outer + inset), painted back-to-front. Empty = none. Not inherited.
    pub box_shadows: Vec<BoxShadow>,
    /// A composed 2D affine `transform` `[a b c d e f]` (maps (x,y)→(a*x+c*y+e, b*x+d*y+f)),
    /// expressed in the box's local coordinate space *before* origin adjustment. `None` = identity
    /// (no transform). A paint-time remap only; does not affect layout. Not inherited.
    pub transform: Option<[f32; 6]>,
    /// `transform-origin` as fractions of the box's own size (x, y); default (0.5, 0.5). Used to
    /// pivot the `transform`. Not inherited.
    pub transform_origin: (f32, f32),

    /// A `mask-image` / `mask` / `-webkit-mask` source (the icon technique), if any. `None` = no
    /// mask. When set, the painter composites the box's background/content only through the mask's
    /// opaque pixels. Not inherited.
    pub mask_image: Option<MaskImage>,

    /// The resolved `content` string for a generated pseudo-element box. `None` for ordinary
    /// elements and for pseudo-elements whose `content` is `none`/`normal`/unsupported (no box).
    pub content: Option<String>,
    /// Computed style of the `::before` pseudo-element, set only when a matching `::before` rule
    /// supplied a `content`. Boxed to keep `ComputedStyle` small. Inherits from this element.
    pub before: Option<Box<ComputedStyle>>,
    /// Computed style of the `::after` pseudo-element (see [`before`](Self::before)).
    pub after: Option<Box<ComputedStyle>>,

    /// The CSS `color-scheme` value for this element. Inherits (initial `Normal`). Only the
    /// root's value is used — [`cascade`] reads it off `<html>` (falling back to `<body>` /
    /// `<meta name="color-scheme">`) and combines it with the OS appearance to decide whether the
    /// page opts into a dark UA canvas/text (see [`ColorScheme::resolves_dark`] and
    /// [`root_used_scheme_dark`]).
    pub color_scheme: ColorScheme,

    /// This element's resolved custom properties (`--name` -> value), case-sensitive. Populated
    /// from the cascade's `var` environment so `getComputedStyle(el).getPropertyValue("--x")`
    /// can read them. Not enumerated by [`property_names`](Self::property_names).
    pub custom_props: HashMap<String, String>,
}

/// Parsed CSS `color-scheme` value. The property lists the schemes a page supports; the browser
/// then picks one (here, light vs dark) for UA-rendered surfaces (canvas background, default text).
/// We model only the three states our UA theming cares about; the `only` keyword and any unknown
/// custom idents are ignored (they don't change which of light/dark we can pick).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorScheme {
    /// `normal` or unset — no opt-in; UA renders light.
    #[default]
    Normal,
    /// `light` (only light supported) — always light.
    Light,
    /// `dark` (only dark supported) — always dark.
    Dark,
    /// `light dark` / `dark light` (both supported) — follow the OS appearance.
    LightDark,
}

impl ColorScheme {
    /// Resolve to a used scheme (true = dark) given the OS appearance (`os_dark`):
    /// `Dark` → dark; `Light`/`Normal` → light; `LightDark` → follow the OS.
    pub fn resolves_dark(self, os_dark: bool) -> bool {
        match self {
            ColorScheme::Dark => true,
            ColorScheme::Light | ColorScheme::Normal => false,
            ColorScheme::LightDark => os_dark,
        }
    }
}

/// Parse a CSS `color-scheme` value (lowercased). Accepts `normal`, and one or both of
/// `light`/`dark` in any order with an optional `only` keyword and unknown custom idents (both
/// ignored). Returns `None` for empty/`inherit`-like input so the caller keeps the existing value.
/// `normal` mixed with `light`/`dark` is treated as the light/dark selection (the keywords win).
fn parse_color_scheme(val: &str) -> Option<ColorScheme> {
    let mut light = false;
    let mut dark = false;
    let mut saw_any = false;
    for tok in val.split_whitespace() {
        saw_any = true;
        match tok {
            "light" => light = true,
            "dark" => dark = true,
            // `only`, `normal`, and unknown custom idents don't change which scheme we can pick.
            _ => {}
        }
    }
    if !saw_any {
        return None;
    }
    Some(match (light, dark) {
        (true, true) => ColorScheme::LightDark,
        (false, true) => ColorScheme::Dark,
        (true, false) => ColorScheme::Light,
        // Only `normal`/`only`/unknown idents → no light/dark opt-in.
        (false, false) => ColorScheme::Normal,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

/// CSS `border-collapse`. `Separate` (default) keeps the classic spaced/double-bordered model;
/// `Collapse` makes adjacent cells share a single border edge (a clean single-line grid). Inherits
/// from the table down to its cells so the layout/painter can read it off any cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BorderCollapse {
    #[default]
    Separate,
    Collapse,
}

/// `vertical-align` for inline-level content. Only the `sub`/`super` keywords (subscript /
/// superscript) are modeled; everything else is treated as `Baseline`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerticalAlign {
    #[default]
    Baseline,
    Sub,
    Super,
}

/// An RGBA color used by gradients and shadows (where alpha is significant, unlike the opaque
/// `(u8,u8,u8)` used elsewhere). Channels are 0..=255.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

/// A resolved gradient color stop: a color and its position as a 0..1 fraction of the gradient
/// line (distributed evenly when the author omits positions).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GradientStop {
    pub color: Rgba,
    /// Position along the gradient line, 0.0..=1.0.
    pub pos: f32,
}

/// A `background-image` gradient. Simplifications: `radial-gradient` is always a centered circle
/// whose radius is the box's half-diagonal (size keywords and `at <position>` are ignored);
/// `repeating-linear-gradient` is treated as a plain `linear-gradient`.
#[derive(Debug, Clone, PartialEq)]
pub enum Gradient {
    /// A linear gradient. `angle_deg` follows the CSS convention: 0deg = to top, 90deg = to
    /// right, 180deg = to bottom. Stops are sorted by `pos` ascending, each in 0..=1.
    Linear { angle_deg: f32, stops: Vec<GradientStop> },
    /// A radial gradient (centered circle, half-diagonal radius). Stops sorted by `pos`.
    Radial { stops: Vec<GradientStop> },
}

/// How a `mask-image` is scaled to the box. Parsed from the `/ <size>` part of the `mask`
/// shorthand. `Contain`/`Cover` honor the mask's aspect ratio; `Stretch` (the default when no
/// size keyword is given) fits the mask to the border box. (Pixel sizes and `auto` collapse to
/// `Contain`, the common icon case.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MaskSize {
    /// Scale the mask to fit inside the border box, preserving aspect ratio (letterboxed).
    #[default]
    Contain,
    /// Scale the mask to cover the border box, preserving aspect ratio (cropped).
    Cover,
    /// Stretch the mask to exactly fill the border box (ignore aspect ratio).
    Stretch,
}

/// A resolved `mask-image` / `mask` / `-webkit-mask` source. Only the image `url(...)` source is
/// modeled (after `var()` resolution): a `data:` URL (percent-encoded or base64 SVG / raster) or a
/// same-origin relative/absolute URL the engine fetches like an `<img>`. `size` records the
/// `contain`/`cover` keyword; position is assumed `center` (the icon convention). Out of scope:
/// gradients-as-mask, multiple comma-separated images (first only), `mask-mode: luminance`,
/// `mask-composite`, and `<mask>`-element references — all treated as alpha masks / no-ops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskImage {
    /// The image source `url(...)` contents, with surrounding quotes stripped and `var()` already
    /// resolved. Either a `data:` URL or a relative/absolute URL to fetch.
    pub url: String,
    /// How the mask is scaled to the box.
    pub size: MaskSize,
}

/// A single `box-shadow` layer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoxShadow {
    /// Inner (`inset`) shadow vs outer (default).
    pub inset: bool,
    /// Horizontal offset (px).
    pub dx: f32,
    /// Vertical offset (px).
    pub dy: f32,
    /// Blur radius (px); 0 = hard edge.
    pub blur: f32,
    /// Spread radius (px); inflates (outer) / deflates (inset) the rect.
    pub spread: f32,
    /// Shadow color (rgba).
    pub color: Rgba,
}

/// CSS `text-transform`. Inherits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextTransform {
    None,
    Uppercase,
    Lowercase,
    Capitalize,
}

/// CSS `white-space` processing mode. Inherits. Controls whitespace collapsing and whether the
/// source newlines / spaces are preserved by layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WhiteSpace {
    /// Collapse runs of whitespace to a single space; newlines are whitespace; wrap as needed.
    #[default]
    Normal,
    /// Collapse whitespace but never wrap (single line).
    Nowrap,
    /// Preserve spaces and newlines; do NOT wrap (newlines are forced breaks).
    Pre,
    /// Preserve spaces and newlines; DO wrap long lines too.
    PreWrap,
}

impl WhiteSpace {
    /// Whether runs of spaces are preserved (not collapsed) under this mode.
    pub fn preserves_spaces(self) -> bool {
        matches!(self, WhiteSpace::Pre | WhiteSpace::PreWrap)
    }
    /// Whether `\n` in the source is a forced line break under this mode.
    pub fn preserves_newlines(self) -> bool {
        matches!(self, WhiteSpace::Pre | WhiteSpace::PreWrap)
    }
}

/// CSS `list-style-type`: the marker drawn before a `display: list-item` box. Inherits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListStyleType {
    /// A filled bullet `•` (the `ul` default).
    #[default]
    Disc,
    /// A hollow bullet `◦`.
    Circle,
    /// A filled square `▪`.
    Square,
    /// `1.`, `2.`, `3.` … (the `ol` default).
    Decimal,
    /// No marker.
    None,
}

/// A length that may be a fixed px value or a percentage of the containing block. Used for
/// min/max sizing constraints so percentages can be resolved in layout (like `width`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeConstraint {
    /// A resolved pixel value.
    Px(f32),
    /// A percentage of the containing block's size (0..=100 → 0.0..=1.0 already divided here).
    Pct(f32),
}

impl SizeConstraint {
    /// Resolve to px given the containing block size along the relevant axis.
    pub fn resolve(&self, basis: f32) -> f32 {
        match self {
            SizeConstraint::Px(v) => *v,
            SizeConstraint::Pct(p) => basis * p,
        }
    }
}

impl Default for ComputedStyle {
    fn default() -> Self {
        ComputedStyle {
            // Initial text color: black on a light canvas, or a light grey when the root opted into
            // a dark `color-scheme` (resolved before the cascade — see `root_used_scheme_dark`). The
            // root inherits this, so every box gets themed default text unless author CSS overrides.
            color: ua_default_text_color(),
            background_color: None,
            font_size: 16.0,
            font_family: None,
            bold: false,
            italic: false,
            text_align: TextAlign::Left,
            display_none: false,
            display_block: false,
            display: Display::Inline,
            position: Position::Static,
            top: None,
            right: None,
            bottom: None,
            left: None,
            top_spec: InsetValue::Auto,
            right_spec: InsetValue::Auto,
            bottom_spec: InsetValue::Auto,
            left_spec: InsetValue::Auto,
            z_index: None,
            width: None,
            height: None,
            min_width: None,
            max_width: None,
            min_height: None,
            max_height: None,
            margin: Edges::default(),
            padding: Edges::default(),
            border: Edges::default(),
            border_color: (0, 0, 0), // initial border-color is currentColor (black)
            overflow_scrollport: false,
            border_collapse: BorderCollapse::Separate,
            border_spacing: 0.0,
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::NoWrap,
            justify_content: JustifyContent::FlexStart,
            align_items: AlignItems::Stretch,
            align_content: None,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: None,
            align_self: AlignSelf::Auto,
            order: 0,
            row_gap: 0.0,
            column_gap: 0.0,
            grid_template_columns: Vec::new(),
            grid_template_rows: Vec::new(),
            grid_column: None,
            grid_row: None,
            line_height: None,
            text_transform: TextTransform::None,
            letter_spacing: 0.0,
            white_space: WhiteSpace::Normal,
            list_style_type: ListStyleType::Disc,
            underline: false,
            line_through: false,
            overline: false,
            vertical_align: VerticalAlign::Baseline,
            opacity: 1.0,
            border_radius: 0.0,
            background_gradient: None,
            box_shadows: Vec::new(),
            transform: None,
            transform_origin: (0.5, 0.5),
            mask_image: None,
            content: None,
            before: None,
            after: None,
            color_scheme: ColorScheme::Normal,
            custom_props: HashMap::new(),
        }
    }
}

/// Format a number the way `getComputedStyle` does: an integer with no decimal point when whole
/// (`16` not `16.0`), otherwise the shortest decimal (`12.5`). Negative zero normalizes to `0`.
fn num(v: f32) -> String {
    let v = if v == 0.0 { 0.0 } else { v }; // normalize -0
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        // Trim trailing zeros from a fixed rendering.
        let mut s = format!("{v:.4}");
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
        s
    }
}

/// Format a length in CSS px (`<n>px`).
fn px(v: f32) -> String {
    format!("{}px", num(v))
}

/// Format a CSS-px length the same way the CSSOM resolved-value serializer does (`<n>px`, trimming
/// trailing zeros). Public so consumers (e.g. the JS `getComputedStyle` layer feeding engine-pushed
/// used values) serialize insets/margins identically to the in-crate paths.
pub fn serialize_px(v: f32) -> String {
    px(v)
}

/// Format an opaque color as `rgb(r, g, b)`.
fn rgb_str((r, g, b): (u8, u8, u8)) -> String {
    format!("rgb({r}, {g}, {b})")
}

impl ComputedStyle {
    /// Return the *computed* value of CSS property `name` (kebab-case) as the string
    /// [`getComputedStyle`](https://developer.mozilla.org/en-US/docs/Web/API/Window/getComputedStyle)
    /// would return for it, for every field this `ComputedStyle` tracks. Properties this struct does
    /// not model return `""` (empty) — which correctly reports "we don't support/track that" to the
    /// feature-detection that drives most callers (e.g. browserscore.dev reads
    /// `getComputedStyle(probe).someProp` and checks whether it's empty).
    ///
    /// Both common longhands and the few cheaply-assembled shorthands (`margin`, `padding`,
    /// `border-width`, `inset`, `gap`) are mapped.
    pub fn get_property(&self, name: &str) -> String {
        // Custom properties are case-sensitive and read straight from the resolved environment.
        let trimmed = name.trim();
        if trimmed.starts_with("--") {
            return self.custom_props.get(trimmed).cloned().unwrap_or_default();
        }
        // Normalize: lowercase + trim (callers pass kebab-case, but be defensive).
        let name = trimmed.to_ascii_lowercase();
        match name.as_str() {
            // --- display / box model mode ---
            "display" => match self.display {
                Display::None => "none",
                Display::Block => "block",
                Display::Inline => "inline",
                Display::InlineBlock => "inline-block",
                Display::Flex => "flex",
                Display::InlineFlex => "inline-flex",
                Display::Grid => "grid",
                Display::InlineGrid => "inline-grid",
                Display::Table => "table",
                Display::TableRow => "table-row",
                Display::TableCell => "table-cell",
                Display::TableRowGroup => "table-row-group",
                Display::TableHeaderGroup => "table-header-group",
                Display::TableFooterGroup => "table-footer-group",
                Display::TableCaption => "table-caption",
                Display::TableColumn => "table-column",
                Display::TableColumnGroup => "table-column-group",
            }
            .to_string(),
            "position" => match self.position {
                Position::Static => "static",
                Position::Relative => "relative",
                Position::Absolute => "absolute",
                Position::Fixed => "fixed",
                Position::Sticky => "sticky",
            }
            .to_string(),
            // `content` is meaningful for pseudo-elements; `None` (no generated content) serializes
            // as the initial `normal`, otherwise as a quoted string.
            "content" => match &self.content {
                Some(s) => serialize_css_string(s),
                None => "normal".to_string(),
            },

            // --- color / paint ---
            "color" => rgb_str(self.color),
            "background-color" => match self.background_color {
                Some(c) => rgb_str(c),
                None => "rgba(0, 0, 0, 0)".to_string(), // CSS transparent
            },
            "border-top-color" | "border-right-color" | "border-bottom-color"
            | "border-left-color" | "border-color"
            // Logical border colors resolve (in the default horizontal-tb / ltr writing mode) to the
            // same physical border color.
            | "border-block-start-color" | "border-block-end-color"
            | "border-inline-start-color" | "border-inline-end-color" => rgb_str(self.border_color),
            // caret-color (auto) and outline-color (currentColor) resolve to the used color value.
            "caret-color" | "outline-color" => rgb_str(self.color),
            "color-scheme" => match self.color_scheme {
                ColorScheme::Normal => "normal",
                ColorScheme::Light => "light",
                ColorScheme::Dark => "dark",
                ColorScheme::LightDark => "light dark",
            }
            .to_string(),
            "opacity" => num(self.opacity),
            "border-radius" => px(self.border_radius),

            // --- typography ---
            "font-family" => self.font_family.clone().unwrap_or_default(),
            "font-size" => px(self.font_size),
            "font-weight" => if self.bold { "700" } else { "400" }.to_string(),
            "font-style" => if self.italic { "italic" } else { "normal" }.to_string(),
            // `direction` / `unicode-bidi` are not modeled beyond their initial values (they don't
            // affect our layout); report the initial computed keyword so getComputedStyle reads work.
            "direction" => "ltr".to_string(),
            "unicode-bidi" => "normal".to_string(),
            "text-align" => match self.text_align {
                TextAlign::Left => "left",
                TextAlign::Center => "center",
                TextAlign::Right => "right",
            }
            .to_string(),
            "text-transform" => match self.text_transform {
                TextTransform::None => "none",
                TextTransform::Uppercase => "uppercase",
                TextTransform::Lowercase => "lowercase",
                TextTransform::Capitalize => "capitalize",
            }
            .to_string(),
            "letter-spacing" => {
                if self.letter_spacing == 0.0 {
                    "normal".to_string()
                } else {
                    px(self.letter_spacing)
                }
            }
            "line-height" => match self.line_height {
                Some(v) => px(v),
                None => "normal".to_string(),
            },
            "white-space" => match self.white_space {
                WhiteSpace::Normal => "normal",
                WhiteSpace::Nowrap => "nowrap",
                WhiteSpace::Pre => "pre",
                WhiteSpace::PreWrap => "pre-wrap",
            }
            .to_string(),
            "list-style-type" => match self.list_style_type {
                ListStyleType::Disc => "disc",
                ListStyleType::Circle => "circle",
                ListStyleType::Square => "square",
                ListStyleType::Decimal => "decimal",
                ListStyleType::None => "none",
            }
            .to_string(),
            "text-decoration-line" | "text-decoration" => {
                let mut parts = Vec::new();
                if self.underline {
                    parts.push("underline");
                }
                if self.line_through {
                    parts.push("line-through");
                }
                if self.overline {
                    parts.push("overline");
                }
                if parts.is_empty() {
                    "none".to_string()
                } else {
                    parts.join(" ")
                }
            }
            "vertical-align" => match self.vertical_align {
                VerticalAlign::Baseline => "baseline",
                VerticalAlign::Sub => "sub",
                VerticalAlign::Super => "super",
            }
            .to_string(),

            // --- sizing ---
            "width" => self.width.map(px).unwrap_or_else(|| "auto".to_string()),
            "height" => self.height.map(px).unwrap_or_else(|| "auto".to_string()),
            "min-width" => self.min_width.map(|c| size_constraint_str(c)).unwrap_or_else(|| "auto".to_string()),
            "min-height" => self.min_height.map(|c| size_constraint_str(c)).unwrap_or_else(|| "auto".to_string()),
            "max-width" => self.max_width.map(|c| size_constraint_str(c)).unwrap_or_else(|| "none".to_string()),
            "max-height" => self.max_height.map(|c| size_constraint_str(c)).unwrap_or_else(|| "none".to_string()),

            // --- insets (position offsets) ---
            "top" => self.top.map(px).unwrap_or_else(|| "auto".to_string()),
            "right" => self.right.map(px).unwrap_or_else(|| "auto".to_string()),
            "bottom" => self.bottom.map(px).unwrap_or_else(|| "auto".to_string()),
            "left" => self.left.map(px).unwrap_or_else(|| "auto".to_string()),
            "inset" => format!(
                "{} {} {} {}",
                self.top.map(px).unwrap_or_else(|| "auto".to_string()),
                self.right.map(px).unwrap_or_else(|| "auto".to_string()),
                self.bottom.map(px).unwrap_or_else(|| "auto".to_string()),
                self.left.map(px).unwrap_or_else(|| "auto".to_string()),
            ),
            "z-index" => self.z_index.map(|z| z.to_string()).unwrap_or_else(|| "auto".to_string()),

            // --- margin ---
            "margin-top" => px(self.margin.top),
            "margin-right" => px(self.margin.right),
            "margin-bottom" => px(self.margin.bottom),
            "margin-left" => px(self.margin.left),
            "margin" => edges_str(self.margin),

            // --- padding ---
            "padding-top" => px(self.padding.top),
            "padding-right" => px(self.padding.right),
            "padding-bottom" => px(self.padding.bottom),
            "padding-left" => px(self.padding.left),
            "padding" => edges_str(self.padding),

            // --- border widths ---
            "border-top-width" => px(self.border.top),
            "border-right-width" => px(self.border.right),
            "border-bottom-width" => px(self.border.bottom),
            "border-left-width" => px(self.border.left),
            "border-width" => edges_str(self.border),

            // --- table ---
            "border-collapse" => match self.border_collapse {
                BorderCollapse::Separate => "separate",
                BorderCollapse::Collapse => "collapse",
            }
            .to_string(),
            "border-spacing" => px(self.border_spacing),

            // --- flex container ---
            "flex-direction" => match self.flex_direction {
                FlexDirection::Row => "row",
                FlexDirection::RowReverse => "row-reverse",
                FlexDirection::Column => "column",
                FlexDirection::ColumnReverse => "column-reverse",
            }
            .to_string(),
            "flex-wrap" => match self.flex_wrap {
                FlexWrap::NoWrap => "nowrap",
                FlexWrap::Wrap => "wrap",
                FlexWrap::WrapReverse => "wrap-reverse",
            }
            .to_string(),
            "justify-content" => justify_content_str(self.justify_content).to_string(),
            "align-items" => match self.align_items {
                AlignItems::Stretch => "stretch",
                AlignItems::FlexStart => "flex-start",
                AlignItems::FlexEnd => "flex-end",
                AlignItems::Center => "center",
                AlignItems::Baseline => "baseline",
            }
            .to_string(),
            "align-content" => match self.align_content {
                Some(jc) => justify_content_str(jc).to_string(),
                None => "normal".to_string(),
            },

            // --- flex item ---
            "flex-grow" => num(self.flex_grow),
            "flex-shrink" => num(self.flex_shrink),
            "flex-basis" => self.flex_basis.map(px).unwrap_or_else(|| "auto".to_string()),
            "align-self" => match self.align_self {
                AlignSelf::Auto => "auto",
                AlignSelf::Stretch => "stretch",
                AlignSelf::FlexStart => "flex-start",
                AlignSelf::FlexEnd => "flex-end",
                AlignSelf::Center => "center",
                AlignSelf::Baseline => "baseline",
            }
            .to_string(),
            "order" => self.order.to_string(),

            // --- gaps ---
            "row-gap" => px(self.row_gap),
            "column-gap" => px(self.column_gap),
            "gap" => {
                if self.row_gap == self.column_gap {
                    px(self.row_gap)
                } else {
                    format!("{} {}", px(self.row_gap), px(self.column_gap))
                }
            }

            // --- grid ---
            "grid-template-columns" => tracks_str(&self.grid_template_columns),
            "grid-template-rows" => tracks_str(&self.grid_template_rows),

            // --- border / font shorthands (resolved value; not enumerated by property_names) ---
            // Each per-side `border-*` shorthand resolves to `<width> <style> <color>`.
            "border-top" => format!("{} none {}", px(self.border.top), rgb_str(self.border_color)),
            "border-right" => format!("{} none {}", px(self.border.right), rgb_str(self.border_color)),
            "border-bottom" => format!("{} none {}", px(self.border.bottom), rgb_str(self.border_color)),
            "border-left" => format!("{} none {}", px(self.border.left), rgb_str(self.border_color)),
            "border" => format!("{} none {}", px(self.border.top), rgb_str(self.border_color)),
            "box-shadow" => {
                if self.box_shadows.is_empty() {
                    "none".to_string()
                } else {
                    self.box_shadows
                        .iter()
                        .map(|s| {
                            // Computed form: `<color> <dx> <dy> <blur> [<spread>] [inset]`. The color
                            // is serialized first (matches resolved-value serialization).
                            let color = if s.color.a == 255 {
                                format!("rgb({}, {}, {})", s.color.r, s.color.g, s.color.b)
                            } else {
                                format!(
                                    "rgba({}, {}, {}, {})",
                                    s.color.r,
                                    s.color.g,
                                    s.color.b,
                                    num(s.color.a as f32 / 255.0)
                                )
                            };
                            let mut out = format!("{} {} {} {}", color, px(s.dx), px(s.dy), px(s.blur));
                            if s.spread != 0.0 {
                                out.push_str(&format!(" {}", px(s.spread)));
                            }
                            if s.inset {
                                out.push_str(" inset");
                            }
                            out
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            }
            "font" => format!(
                "{} {} / {} sans-serif",
                if self.italic { "italic" } else { "normal" },
                px(self.font_size),
                match self.line_height { Some(v) => px(v), None => "normal".to_string() },
            ),

            // Anything else this struct does not model: report empty so feature detection sees
            // "unsupported/untracked" (which is the correct, honest answer for those callers).
            _ => String::new(),
        }
    }

    /// The CSSOM ["resolved value"](https://drafts.csswg.org/cssom/#resolved-value) of an inset
    /// longhand (`side` ∈ {top,right,bottom,left}), per the *property-like* `top`/`right`/`bottom`/
    /// `left` special-case.
    ///
    /// - `box_less` (display:none / no rendered box) or `position: static` → the **computed** value:
    ///   lengths absolutized to px, percentages preserved, `auto` preserved.
    /// - `position: sticky` → like static but percentages resolve against the containing block
    ///   (`basis`); `auto` is preserved.
    /// - `position: relative` → a used px length: a set side resolves against `basis`; an `auto`
    ///   side mirrors the negated opposite (or `0` when both are `auto`).
    /// - `position: absolute`/`fixed` → the used px value. Set sides and the over-constrained
    ///   "auto vs set" pairing (`basis − opposite`) are resolved here; the all-`auto` static-position
    ///   case needs layout we don't have synchronously, so it falls back to `0` (documented gap).
    ///
    /// `basis` is the containing-block extent on this side's axis (height for top/bottom, width for
    /// left/right); pass `f32::NAN` when unknown (box-less / static, where it's unused).
    pub fn resolved_inset(&self, side: EdgeSide, box_less: bool, basis: f32) -> String {
        let (spec, opposite) = match side {
            EdgeSide::Top => (self.top_spec, self.bottom_spec),
            EdgeSide::Bottom => (self.bottom_spec, self.top_spec),
            EdgeSide::Left => (self.left_spec, self.right_spec),
            EdgeSide::Right => (self.right_spec, self.left_spec),
            EdgeSide::All => return String::new(),
        };

        // No box, or insets that don't apply (static): the computed (specified) value.
        if box_less || self.position == Position::Static {
            return spec.serialize_specified();
        }

        match self.position {
            // Sticky preserves `auto`; otherwise resolve (percentages against the cb).
            Position::Sticky => match spec {
                InsetValue::Auto => "auto".to_string(),
                _ => px(spec.resolve_px(basis).unwrap_or(0.0)),
            },
            // Relative: opposite-pair auto rules, everything used-px.
            Position::Relative => {
                let used = match spec {
                    InsetValue::Auto => match opposite.resolve_px(basis) {
                        Some(o) => -o, // start auto, end set → mirror the negated opposite
                        None => 0.0,   // both auto → 0
                    },
                    _ => spec.resolve_px(basis).unwrap_or(0.0),
                };
                px(used)
            }
            // Absolute / fixed: resolve what we can without layout.
            Position::Absolute | Position::Fixed => {
                let used = match spec {
                    InsetValue::Auto => match opposite.resolve_px(basis) {
                        // Over-constrained "auto vs set": stretch to fill (basis − opposite).
                        Some(o) => basis - o,
                        // Both auto → static position; needs layout we lack. Approximate as 0.
                        None => 0.0,
                    },
                    _ => spec.resolve_px(basis).unwrap_or(0.0),
                };
                px(used)
            }
            Position::Static => unreachable!(),
        }
    }

    /// The CSS property names this `ComputedStyle` can return a (non-empty) value for, in a stable
    /// order. Backs `getComputedStyle(el).length`/`item(i)`/index access/iteration. Shorthands are
    /// included (browsers enumerate them too).
    pub fn property_names(&self) -> Vec<&'static str> {
        const NAMES: &[&str] = &[
            "display",
            "position",
            "color",
            "background-color",
            "border-color",
            "border-top-color",
            "border-right-color",
            "border-bottom-color",
            "border-left-color",
            "border-collapse",
            "border-spacing",
            "opacity",
            "border-radius",
            "font-size",
            "font-weight",
            "font-style",
            "text-align",
            "text-transform",
            "letter-spacing",
            "line-height",
            "white-space",
            "list-style-type",
            "text-decoration",
            "text-decoration-line",
            "vertical-align",
            "width",
            "height",
            "min-width",
            "min-height",
            "max-width",
            "max-height",
            "top",
            "right",
            "bottom",
            "left",
            "inset",
            "z-index",
            "margin",
            "margin-top",
            "margin-right",
            "margin-bottom",
            "margin-left",
            "padding",
            "padding-top",
            "padding-right",
            "padding-bottom",
            "padding-left",
            "border-width",
            "border-top-width",
            "border-right-width",
            "border-bottom-width",
            "border-left-width",
            "flex-direction",
            "flex-wrap",
            "justify-content",
            "align-items",
            "align-content",
            "flex-grow",
            "flex-shrink",
            "flex-basis",
            "align-self",
            "order",
            "row-gap",
            "column-gap",
            "gap",
            "grid-template-columns",
            "grid-template-rows",
            // `direction` and `unicode-bidi` are the two properties NOT reset by the `all` shorthand;
            // getComputedStyle enumerates them (with their inherited/initial keyword) so author code
            // iterating computed longhands sees them. Their values come from `get_property` below.
            "direction",
            "unicode-bidi",
        ];
        // Every name here maps to a tracked field, so all are non-empty.
        NAMES.to_vec()
    }
}

/// True if a quoted `<family-name>` body must stay quoted (it would otherwise be reinterpreted as a
/// generic-family / CSS-wide keyword / `default`).
fn is_reserved_font_family_word(body: &str) -> bool {
    matches!(
        body.to_ascii_lowercase().as_str(),
        "serif" | "sans-serif" | "cursive" | "fantasy" | "monospace" | "system-ui" | "math"
            | "ui-serif" | "ui-sans-serif" | "ui-monospace" | "ui-rounded"
            | "default" | "inherit" | "initial" | "unset" | "revert" | "revert-layer"
    )
}

fn is_css_ident_word(w: &str) -> bool {
    let mut chars = w.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    // Optional leading hyphen.
    let (first, rest_from): (char, bool) = if first == '-' {
        match chars.next() {
            Some(c) => (c, true),
            None => return false,
        }
    } else {
        (first, false)
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    let _ = rest_from;
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return false;
        }
    }
    true
}

/// Serialize a `font-family` list to CSSOM canonical form: split on top-level commas, normalize the
/// quoting of each family name (unquote a quoted name only when its body is a single-space-separated
/// sequence of valid CSS identifiers that round-trips and isn't a reserved word), and join with
/// `, `.
pub fn serialize_font_family(val: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for part in split_top_level_commas(val) {
        let fam = part.trim();
        if fam.is_empty() {
            continue;
        }
        let first = fam.chars().next().unwrap();
        if first == '"' || first == '\'' {
            let body: String = fam.chars().skip(1).take(fam.chars().count().saturating_sub(2)).collect();
            let words: Vec<&str> = body.split(' ').collect();
            let all_ident = !words.is_empty() && words.iter().all(|w| is_css_ident_word(w));
            let round_trips = all_ident && words.join(" ") == body;
            if round_trips && !is_reserved_font_family_word(&body) {
                out.push(body);
            } else {
                out.push(format!("\"{}\"", body.replace('\\', "\\\\").replace('"', "\\\"")));
            }
        } else {
            // Unquoted: collapse internal whitespace runs to single spaces.
            let collapsed: Vec<&str> = fam.split_whitespace().collect();
            out.push(collapsed.join(" "));
        }
    }
    out.join(", ")
}

/// Split a string on top-level commas (not inside parens or quotes).
fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    let mut buf = String::new();
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = quote {
            buf.push(c);
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                buf.push(c);
            }
            '(' => {
                depth += 1;
                buf.push(c);
            }
            ')' => {
                if depth > 0 {
                    depth -= 1;
                }
                buf.push(c);
            }
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut buf));
            }
            _ => buf.push(c),
        }
        i += 1;
    }
    out.push(buf);
    out
}

/// Serialize a string value as a CSS `<string>` (double-quoted, with `"` and `\` escaped) — the
/// form `getComputedStyle(...).content` returns for a pseudo-element's generated text.
fn serialize_css_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn size_constraint_str(c: SizeConstraint) -> String {
    match c {
        SizeConstraint::Px(v) => px(v),
        SizeConstraint::Pct(p) => format!("{}%", num(p * 100.0)),
    }
}

fn justify_content_str(jc: JustifyContent) -> &'static str {
    match jc {
        JustifyContent::FlexStart => "flex-start",
        JustifyContent::FlexEnd => "flex-end",
        JustifyContent::Center => "center",
        JustifyContent::SpaceBetween => "space-between",
        JustifyContent::SpaceAround => "space-around",
        JustifyContent::SpaceEvenly => "space-evenly",
    }
}

/// Serialize four edges the way `getComputedStyle` returns the shorthand: collapsed when sides are
/// equal, otherwise the full `top right bottom left` form.
fn edges_str(e: Edges) -> String {
    if e.top == e.right && e.right == e.bottom && e.bottom == e.left {
        px(e.top)
    } else if e.top == e.bottom && e.left == e.right {
        format!("{} {}", px(e.top), px(e.right))
    } else {
        format!("{} {} {} {}", px(e.top), px(e.right), px(e.bottom), px(e.left))
    }
}

fn track_str(t: TrackSize) -> String {
    match t {
        TrackSize::Px(v) => px(v),
        TrackSize::Fr(v) => format!("{}fr", num(v)),
        TrackSize::Pct(p) => format!("{}%", num(p)),
        TrackSize::Auto => "auto".to_string(),
    }
}

fn tracks_str(tracks: &[TrackSize]) -> String {
    if tracks.is_empty() {
        return "none".to_string();
    }
    tracks.iter().map(|t| track_str(*t)).collect::<Vec<_>>().join(" ")
}

/// Compute a [`ComputedStyle`] for every element node in `doc`, using the built-in UA
/// stylesheet first, then the supplied author `sheets` (in document order), then each
/// element's inline `style="…"` attribute (highest precedence within an element).
pub fn cascade(
    doc: &dom::Document,
    sheets: &[css::Stylesheet],
) -> HashMap<dom::NodeId, ComputedStyle> {
    cascade_locked(doc, sheets, None).0
}

/// Like [`cascade`], but `os_dark` is the OS appearance for this document (true = Dark), applied
/// to `@media (prefers-color-scheme)` and the `color-scheme` resolution *atomically* with the
/// cascade (set under the cascade lock so a concurrent cascade can't clobber the shared flag), and
/// also returns the root's resolved *used* color scheme (true = dark). The engine stores that on
/// its layout cache so the canvas background doesn't have to re-read the racy process-global. Pass
/// this rather than calling [`set_color_scheme_dark`] separately before [`cascade`].
pub fn cascade_with_root_scheme(
    doc: &dom::Document,
    sheets: &[css::Stylesheet],
    os_dark: bool,
) -> (HashMap<dom::NodeId, ComputedStyle>, bool) {
    cascade_locked(doc, sheets, Some(os_dark))
}

/// The shared, lock-held cascade body. When `os_dark` is `Some`, the OS-appearance flag is set
/// under the lock first (so `@media (prefers-color-scheme)` and the `color-scheme` resolution see a
/// stable value); when `None`, the previously-set global is used as-is (back-compat for callers
/// that set it themselves). Returns the styles plus the root's resolved used color scheme.
fn cascade_locked(
    doc: &dom::Document,
    sheets: &[css::Stylesheet],
    os_dark: Option<bool>,
) -> (HashMap<dom::NodeId, ComputedStyle>, bool) {
    // Hold the cascade lock for the whole body: the OS-appearance flag and the root-color-scheme
    // global are written and read back here, so concurrent cascades must not interleave.
    let _cascade_guard = CASCADE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(dark) = os_dark {
        set_color_scheme_dark(dark);
    }
    // Collect `@property` registrations from all author sheets (later sheets / later rules win on
    // duplicate names, matching the cascade-order "last registration wins" behaviour).
    {
        let mut reg: Vec<css::PropertyRule> = Vec::new();
        for sheet in sheets {
            for pr in &sheet.property_rules {
                reg.retain(|r| r.name != pr.name);
                reg.push(pr.clone());
            }
        }
        REGISTERED_PROPERTIES.with(|c| *c.borrow_mut() = reg);
    }
    // Collect `@namespace` bindings from all author sheets (later declarations win on duplicate
    // prefix / default).
    {
        let mut env = NamespaceEnv::default();
        for sheet in sheets {
            for ns in &sheet.namespace_rules {
                if ns.prefix.is_empty() {
                    env.default_ns = Some(ns.uri.clone());
                } else {
                    env.prefixes.retain(|(p, _)| p != &ns.prefix);
                    env.prefixes.push((ns.prefix.clone(), ns.uri.clone()));
                }
            }
        }
        NAMESPACE_BINDINGS.with(|c| *c.borrow_mut() = env);
    }
    let mut out = HashMap::new();
    // Pre-pass: resolve the root's *used* color scheme (light vs dark) BEFORE the real UA sheet and
    // cascade are built, so the UA dark defaults (html/body text color in `user_agent_stylesheet`,
    // initial color via `ComputedStyle::default()`, canvas background via `ua_default_canvas_color`)
    // are seeded for this whole cascade. `color-scheme` is often gated behind
    // `@media (prefers-color-scheme: dark)`, so we cascade `<html>` (then `<body>`) for real to read
    // the property, then combine with the OS flag. `<meta name="color-scheme">` is a fallback opt-in
    // mapped like the property. See `resolve_root_color_scheme` (it runs with light defaults so its
    // own read can't depend on the result).
    set_root_used_scheme_dark(resolve_root_color_scheme(doc, sheets));
    // Now build the (themed) UA sheet and the selector index ONCE over UA + author sheets, so every
    // node shares it instead of re-scanning (and re-parsing) all rules per element.
    let ua = user_agent_stylesheet();
    let index = SelectorIndex::build(&ua, sheets);
    // The root inherits from a fresh default style (now themed by the resolved scheme above).
    let initial = ComputedStyle::default();
    // Custom properties (`--name`) inherit; the root starts with an empty environment.
    let initial_vars: HashMap<String, String> = HashMap::new();
    cascade_node(doc, doc.root(), &initial, &initial_vars, false, &index, &mut out);
    (out, root_used_scheme_dark())
}

/// Resolve the root's *used* color scheme (true = dark) for one cascade. Reads the page's
/// `color-scheme` opt-in (which determines whether the UA renders a dark canvas + light text) and
/// combines it with the OS appearance:
///
/// 1. Cascade `<html>` for real (so a `color-scheme` set under `@media (prefers-color-scheme:dark)`
///    or via `:root{…}` is picked up), then fall back to `<body>` if `<html>` left it `Normal`.
/// 2. If still `Normal`, honor a `<meta name="color-scheme" content="…">` in `<head>`, mapped like
///    the property.
/// 3. Apply [`ColorScheme::resolves_dark`] against the OS flag: only-dark → dark; only-light/normal
///    → light; `light dark` (both) → follow the OS.
///
/// Runs the pre-pass with the dark UA defaults *disabled* (`set_root_used_scheme_dark(false)`) so
/// the property read doesn't depend on its own result. The caller stores the returned value.
fn resolve_root_color_scheme(doc: &dom::Document, sheets: &[css::Stylesheet]) -> bool {
    // Read the property with light defaults so the pre-pass result can't depend on itself.
    set_root_used_scheme_dark(false);
    // Build a (light-themed) UA sheet + index just for this read. `color-scheme` only ever comes
    // from author CSS / inline style / meta, so the UA rules don't affect the result, but we still
    // index over UA + author to match real selectors (e.g. `:root { color-scheme: dark }`).
    let ua = user_agent_stylesheet();
    let index = SelectorIndex::build(&ua, sheets);
    let initial = ComputedStyle::default();
    let initial_vars: HashMap<String, String> = HashMap::new();

    let mut scheme = ColorScheme::Normal;
    if let Some(html) = find_element(doc, "html") {
        if let dom::NodeData::Element(el) = &doc.get(html).data {
            let (s, _) =
                compute_element_style(doc, html, el, &initial, &initial_vars, false, &index);
            scheme = s.color_scheme;
        }
    }
    if scheme == ColorScheme::Normal {
        if let Some(body) = find_element(doc, "body") {
            if let dom::NodeData::Element(el) = &doc.get(body).data {
                let (s, _) =
                    compute_element_style(doc, body, el, &initial, &initial_vars, false, &index);
                scheme = s.color_scheme;
            }
        }
    }
    if scheme == ColorScheme::Normal {
        if let Some(meta) = meta_color_scheme(doc) {
            scheme = meta;
        }
    }
    scheme.resolves_dark(color_scheme_dark())
}

/// Depth-first search for the first element with the given (lowercase) tag name.
fn find_element(doc: &dom::Document, tag: &str) -> Option<dom::NodeId> {
    fn walk(doc: &dom::Document, id: dom::NodeId, tag: &str) -> Option<dom::NodeId> {
        if id.0 >= doc.len() {
            return None;
        }
        if let dom::NodeData::Element(el) = &doc.get(id).data {
            if el.tag.eq_ignore_ascii_case(tag) {
                return Some(id);
            }
        }
        for &c in &doc.get(id).children {
            if let Some(found) = walk(doc, c, tag) {
                return Some(found);
            }
        }
        None
    }
    walk(doc, doc.root(), tag)
}

/// Read a `<meta name="color-scheme" content="…">` (the HTML opt-in equivalent of the CSS
/// property) and map its `content` like `color-scheme`. Returns the first such meta's value.
fn meta_color_scheme(doc: &dom::Document) -> Option<ColorScheme> {
    fn walk(doc: &dom::Document, id: dom::NodeId) -> Option<ColorScheme> {
        if id.0 >= doc.len() {
            return None;
        }
        if let dom::NodeData::Element(el) = &doc.get(id).data {
            if el.tag.eq_ignore_ascii_case("meta")
                && el.attrs.get("name").is_some_and(|n| n.eq_ignore_ascii_case("color-scheme"))
            {
                if let Some(content) = el.attrs.get("content") {
                    if let Some(cs) = parse_color_scheme(&content.to_ascii_lowercase()) {
                        return Some(cs);
                    }
                }
            }
        }
        for &c in &doc.get(id).children {
            if let Some(found) = walk(doc, c) {
                return Some(found);
            }
        }
        None
    }
    walk(doc, doc.root())
}

/// One indexed selector. Points back at the rule's declarations and carries everything needed
/// to confirm a full compound match and slot the result into the cascade ordering.
struct Entry<'a> {
    /// 0 = UA origin, 1 = author origin (matches `MatchEntry.origin`).
    origin: u8,
    /// Global source order, incremented across UA rules then author rules in sheet/rule order
    /// — identical to the `order` the brute-force scan assigns.
    order: usize,
    /// The compiled selector this entry was indexed under (used to verify the full compound).
    compiled: Compiled,
    /// The rule's declarations (applied as a unit when any of its selectors match).
    decls: &'a [(String, String)],
    /// The owning stylesheet's base URL (for resolving relative `url(...)` in `mask-image` etc.
    /// against the stylesheet, not the document). `None` if the sheet was parsed without a base.
    base: Option<&'a str>,
}

/// An index over all UA + author selectors, bucketed most-selective-key-first so a given
/// element only has to test the handful of rules that could plausibly match it (those keyed
/// by its id, one of its classes, its tag, or the universal/`:root` catch-all) instead of
/// every rule in every sheet.
///
/// Built once per [`cascade`]. Rules whose `@media`/`@container` doesn't apply are dropped at
/// build time (those conditions don't depend on the element). Selectors that the matcher would
/// never match (combinators etc.) are dropped entirely.
struct SelectorIndex<'a> {
    by_id: HashMap<String, Vec<Entry<'a>>>,
    by_class: HashMap<String, Vec<Entry<'a>>>,
    by_type: HashMap<String, Vec<Entry<'a>>>,
    universal: Vec<Entry<'a>>,
}

impl<'a> SelectorIndex<'a> {
    fn build(ua: &'a css::Stylesheet, author: &'a [css::Stylesheet]) -> SelectorIndex<'a> {
        let mut idx = SelectorIndex {
            by_id: HashMap::new(),
            by_class: HashMap::new(),
            by_type: HashMap::new(),
            universal: Vec::new(),
        };
        let mut order = 0usize;
        // UA rules first, then author rules — preserving the exact global ordering the
        // brute-force scan assigns (order increments across every rule whether or not it is
        // indexed).
        for rule in &ua.rules {
            idx.add_rule(rule, 0, order);
            order += 1;
        }
        for sheet in author {
            for rule in &sheet.rules {
                idx.add_rule(rule, 1, order);
                order += 1;
            }
        }
        idx
    }

    /// Index every (indexable) selector of one rule, unless its media/container precludes it.
    fn add_rule(&mut self, rule: &'a css::Rule, origin: u8, order: usize) {
        // media/container don't depend on the element, so evaluate once here and skip the
        // whole rule if it doesn't apply (it can never contribute to any element).
        if !(media_applies(rule.media.as_deref()) && container_applies(rule.container.as_deref())) {
            return;
        }
        for sel in &rule.selectors {
            let Some(compiled) = compile_selector(sel) else {
                continue; // unsupported selector (e.g. pseudo-element) — drop it
            };
            // Bucket under the rightmost (subject) compound's most-selective simple part.
            match compiled.bucket_key().clone() {
                BucketKey::Id(id) => self.by_id.entry(id).or_default(),
                BucketKey::Class(class) => self.by_class.entry(class).or_default(),
                BucketKey::Type(t) => self.by_type.entry(t).or_default(),
                BucketKey::Universal => &mut self.universal,
            }
            .push(Entry { origin, order, compiled, decls: &rule.declarations, base: rule.base_url.as_deref() });
        }
    }
}

// Live viewport metrics used to evaluate media queries (`min-width`/`max-width`/resolution),
// `@container` conditions, and viewport units (`vw`/`vh`/`%`) during the cascade. The engine sets
// these via `set_viewport_metrics` before each cascade, so they reflect the real window size and
// backing scale — and because the cascade re-runs on resize, media/container queries and viewport
// units respond to window resizing. Stored as f32 bits in atomics (0 = unset → fall back below).
static VIEWPORT_W_BITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static VIEWPORT_H_BITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static VIEWPORT_DPR_BITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Live OS appearance used to evaluate `@media (prefers-color-scheme: dark|light)` during the
/// cascade. `true` = Dark. The engine sets this via [`set_color_scheme_dark`] on launch and on
/// every Light/Dark toggle; the cascade re-runs (layout cache invalidated) so dark-mode stylesheet
/// rules take effect. Mirrors the same flag in the `js` crate (which drives the JS `matchMedia`).
static COLOR_SCHEME_DARK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set the logical viewport size (CSS px) and device pixel ratio used by the cascade for media
/// queries and viewport units. Call before [`cascade`] whenever the viewport changes.
pub fn set_viewport_metrics(width: f32, height: f32, device_pixel_ratio: f32) {
    use std::sync::atomic::Ordering;
    VIEWPORT_W_BITS.store(width.max(1.0).to_bits(), Ordering::Relaxed);
    VIEWPORT_H_BITS.store(height.max(1.0).to_bits(), Ordering::Relaxed);
    VIEWPORT_DPR_BITS.store(device_pixel_ratio.max(0.1).to_bits(), Ordering::Relaxed);
}

/// Set whether the effective OS appearance is Dark, used to evaluate
/// `@media (prefers-color-scheme: dark|light)` in the cascade. Call before [`cascade`] (the engine
/// does this on launch and on every appearance toggle).
pub fn set_color_scheme_dark(is_dark: bool) {
    COLOR_SCHEME_DARK.store(is_dark, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the effective OS appearance is currently Dark (drives `prefers-color-scheme`).
fn color_scheme_dark() -> bool {
    COLOR_SCHEME_DARK.load(std::sync::atomic::Ordering::Relaxed)
}

/// The root's *used* color scheme (true = dark), resolved by [`cascade`] from the page's
/// `color-scheme` (off `<html>`/`<body>`/`<meta>`) combined with the OS appearance. Seeds the dark
/// UA defaults: the initial/inherited text color ([`ua_default_text_color`]) and the canvas
/// background ([`ua_default_canvas_color`], read by the engine's `page_background`). Defaults to
/// light (false). Re-resolved every cascade, so an OS Light/Dark toggle (which re-runs the cascade)
/// flips both the `@media` gating the page's `color-scheme` AND this used scheme.
static ROOT_USED_SCHEME_DARK: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Serializes [`cascade`] across threads. `cascade` resolves the root color scheme into the
/// process-global [`ROOT_USED_SCHEME_DARK`] and then reads it back while building UA defaults, so
/// two concurrent cascades on different documents could otherwise clobber each other's flag. The
/// engine runs one cascade at a time, so this only matters for parallel `cargo test`; the lock is
/// cheap and held only for the (fast) cascade body. Poisoning is irrelevant — we only need mutual
/// exclusion — so the guard ignores it.
static CASCADE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

thread_local! {
    /// Registered custom properties (`@property`) in effect for the current cascade. Set at the
    /// start of [`cascade_locked`] from the author sheets; read by `compute_element_style` to seed
    /// each element's registered-property initial values into its `custom_props` environment. Only
    /// populated for the duration of one cascade (which holds [`CASCADE_LOCK`], so this is
    /// effectively single-threaded). Keyed by property name (`--x`).
    static REGISTERED_PROPERTIES: std::cell::RefCell<Vec<css::PropertyRule>> =
        const { std::cell::RefCell::new(Vec::new()) };

    /// `@namespace` prefix bindings in effect for the current cascade: `(prefix -> uri)` plus an
    /// optional default namespace (empty prefix). Set at the start of [`cascade_locked`]; read by
    /// `compound_matches` to resolve a selector's namespace component against the element's
    /// namespace. Empty when no `@namespace` rule is present (the common case), in which case
    /// namespace constraints are ignored and matching behaves exactly as before.
    static NAMESPACE_BINDINGS: std::cell::RefCell<NamespaceEnv> =
        const { std::cell::RefCell::new(NamespaceEnv { default_ns: None, prefixes: Vec::new() }) };
}

/// The resolved `@namespace` environment for one cascade.
#[derive(Default, Clone)]
struct NamespaceEnv {
    /// The default namespace URI (`@namespace url(...)` with no prefix), if any.
    default_ns: Option<String>,
    /// `(prefix, uri)` bindings from `@namespace prefix url(...)`.
    prefixes: Vec<(String, String)>,
}

impl NamespaceEnv {
    fn lookup(&self, prefix: &str) -> Option<&str> {
        self.prefixes.iter().find(|(p, _)| p == prefix).map(|(_, u)| u.as_str())
    }
}

/// Whether the root opted into a dark color scheme for this cascade (UA dark canvas + light text).
pub fn root_used_scheme_dark() -> bool {
    ROOT_USED_SCHEME_DARK.load(std::sync::atomic::Ordering::Relaxed)
}

fn set_root_used_scheme_dark(dark: bool) {
    ROOT_USED_SCHEME_DARK.store(dark, std::sync::atomic::Ordering::Relaxed);
}

/// UA initial/default text color: black on a light page, light grey (`#e8e8e8`) when the root used
/// a dark color scheme. Read by `ComputedStyle::default()` (the cascade root's inherited color and
/// the `color: initial`/`unset` reset target), so dark pages get light text without per-element CSS.
fn ua_default_text_color() -> (u8, u8, u8) {
    if root_used_scheme_dark() {
        (0xe8, 0xe8, 0xe8)
    } else {
        (0, 0, 0)
    }
}

/// UA default canvas/page background: white on a light page, dark (`#1e1e1e`) when the root used a
/// dark color scheme. Read by the engine's `page_background` when no html/body `background-color`
/// is set.
pub fn ua_default_canvas_color() -> (u8, u8, u8) {
    if root_used_scheme_dark() {
        (0x1e, 0x1e, 0x1e)
    } else {
        (0xff, 0xff, 0xff)
    }
}

// Live pointer/keyboard interaction state used to evaluate `:hover`/`:focus`/`:active`/
// `:focus-within`/`:focus-visible` during the cascade. The engine sets these via
// `set_interaction_state` before each cascade. We store the hovered/focused node ids (the
// `usize` inside a `dom::NodeId`); `usize::MAX` means "none".
static HOVERED_NODE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);
static FOCUSED_NODE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);

/// Set the currently hovered and focused node ids (as the raw `usize` of their [`dom::NodeId`]),
/// or `None` for neither. Call before [`cascade`] whenever interaction state changes so
/// `:hover`/`:focus`/… re-evaluate. Mirrors [`set_viewport_metrics`].
pub fn set_interaction_state(hovered: Option<usize>, focused: Option<usize>) {
    use std::sync::atomic::Ordering;
    HOVERED_NODE.store(hovered.unwrap_or(usize::MAX), Ordering::Relaxed);
    FOCUSED_NODE.store(focused.unwrap_or(usize::MAX), Ordering::Relaxed);
}

fn interaction_hovered() -> Option<usize> {
    let v = HOVERED_NODE.load(std::sync::atomic::Ordering::Relaxed);
    if v == usize::MAX { None } else { Some(v) }
}
fn interaction_focused() -> Option<usize> {
    let v = FOCUSED_NODE.load(std::sync::atomic::Ordering::Relaxed);
    if v == usize::MAX { None } else { Some(v) }
}

fn viewport_width() -> f32 {
    let b = VIEWPORT_W_BITS.load(std::sync::atomic::Ordering::Relaxed);
    if b == 0 { 1280.0 } else { f32::from_bits(b) }
}
fn viewport_height() -> f32 {
    let b = VIEWPORT_H_BITS.load(std::sync::atomic::Ordering::Relaxed);
    if b == 0 { 800.0 } else { f32::from_bits(b) }
}
fn viewport_dpr() -> f32 {
    let b = VIEWPORT_DPR_BITS.load(std::sync::atomic::Ordering::Relaxed);
    if b == 0 { 2.0 } else { f32::from_bits(b) }
}

/// Viewport width (px) used for `min-width`/`max-width` media queries — the real window width.
fn assumed_viewport_width() -> f32 { viewport_width() }
/// Viewport height (px) used to resolve `vh` units — the real window height.
fn assumed_viewport_height() -> f32 { viewport_height() }
/// Width (px) used to evaluate `@container` conditions. Correct container sizing needs layout
/// (which runs after the cascade), so we approximate with the viewport width.
fn assumed_container_width() -> f32 { viewport_width() }

/// Recursively compute styles. `parent` is the parent's computed style (the inheritance
/// source); `parent_vars` is the set of custom properties inherited from ancestors;
/// `parent_hidden` is true if any ancestor was `display: none`.
#[allow(clippy::too_many_arguments)]
fn cascade_node(
    doc: &dom::Document,
    id: dom::NodeId,
    parent: &ComputedStyle,
    parent_vars: &HashMap<String, String>,
    parent_hidden: bool,
    index: &SelectorIndex,
    out: &mut HashMap<dom::NodeId, ComputedStyle>,
) {
    let node = doc.get(id);
    let (computed, vars) = if let dom::NodeData::Element(el) = &node.data {
        let (style, vars) =
            compute_element_style(doc, id, el, parent, parent_vars, parent_hidden, index);
        out.insert(id, style.clone());
        (style, vars)
    } else {
        // Non-elements inherit the parent style so text runs can read color/size off the
        // nearest element ancestor via the parent passed down.
        (parent.clone(), parent_vars.clone())
    };
    let hidden = parent_hidden || computed.display_none;
    for &child in &node.children {
        // Defensive: skip any child id that points outside the arena. The engine prunes these
        // after JS runs, but guarding here too means a stale id can never panic the renderer.
        if child.0 >= doc.len() {
            continue;
        }
        cascade_node(doc, child, &computed, &vars, hidden, index, out);
    }
}

/// Resolve one element's computed style: gather matching declarations from all origins in
/// precedence order, apply them, then layer inheritance.
#[allow(clippy::too_many_arguments)]
fn compute_element_style<'a>(
    doc: &dom::Document,
    node_id: dom::NodeId,
    el: &dom::ElementData,
    parent: &ComputedStyle,
    parent_vars: &HashMap<String, String>,
    parent_hidden: bool,
    index: &'a SelectorIndex<'a>,
) -> (ComputedStyle, HashMap<String, String>) {
    // Start from inherited values; non-inherited properties get reset below.
    let mut style = ComputedStyle {
        custom_props: HashMap::new(),
        color: parent.color,
        background_color: None, // not inherited
        font_size: parent.font_size,
        font_family: parent.font_family.clone(),
        bold: parent.bold,
        italic: parent.italic,
        text_align: parent.text_align,
        display_none: false, // not inherited
        display_block: false,
        display: Display::Inline,
        position: Position::Static,
        top: None,
        right: None,
        bottom: None,
        left: None,
        top_spec: InsetValue::Auto,
        right_spec: InsetValue::Auto,
        bottom_spec: InsetValue::Auto,
        left_spec: InsetValue::Auto,
        z_index: None,
        // Box properties are not inherited: each element starts from initial values.
        width: None,
        height: None,
        min_width: None,
        max_width: None,
        min_height: None,
        max_height: None,
        margin: Edges::default(),
        padding: Edges::default(),
        border: Edges::default(),
        border_color: parent.color, // initial border-color is currentColor
        overflow_scrollport: false, // not inherited; initial overflow is `visible`
        // border-collapse / border-spacing inherit (set on the table, read by cells).
        border_collapse: parent.border_collapse,
        border_spacing: parent.border_spacing,
        flex_direction: FlexDirection::Row,
        flex_wrap: FlexWrap::NoWrap,
        justify_content: JustifyContent::FlexStart,
        align_items: AlignItems::Stretch,
        align_content: None,
        flex_grow: 0.0,
        flex_shrink: 1.0,
        flex_basis: None,
        align_self: AlignSelf::Auto,
        order: 0,
        row_gap: 0.0,
        column_gap: 0.0,
        grid_template_columns: Vec::new(),
        grid_template_rows: Vec::new(),
        grid_column: None,
        grid_row: None,
        // Typography extras inherit.
        line_height: parent.line_height,
        text_transform: parent.text_transform,
        letter_spacing: parent.letter_spacing,
        white_space: parent.white_space,
        list_style_type: parent.list_style_type,
        underline: parent.underline,
        line_through: parent.line_through,
        overline: parent.overline,
        // `vertical-align` is not inherited; each box starts at the baseline.
        vertical_align: VerticalAlign::Baseline,
        // Paint extras: opacity & border-radius are not inherited.
        opacity: 1.0,
        border_radius: 0.0,
        background_gradient: None,
        box_shadows: Vec::new(),
        transform: None,
        transform_origin: (0.5, 0.5),
        // `mask-image` is not inherited; each box starts unmasked.
        mask_image: None,
        // `content` only applies to generated pseudo-elements; ordinary elements never carry one.
        content: None,
        before: None,
        after: None,
        // `color-scheme` inherits (initial `Normal`).
        color_scheme: parent.color_scheme,
    };
    if parent_hidden {
        style.display_none = true;
        style.display = Display::None;
    }

    // Collect (specificity, source_order, declarations) from every matching rule across all
    // origins. We process origins lowest-precedence-first and rely on a stable sort that puts
    // later, higher-specificity entries last so they win when applied in order.
    struct MatchEntry<'a> {
        origin: u8, // 0 = UA, 1 = presentational hints, 2 = author, 3 = inline
        specificity: u32,
        order: usize,
        decls: &'a [(String, String)],
        /// The owning sheet's base URL, for resolving relative `url(...)` values.
        base: Option<&'a str>,
    }
    let mut matches: Vec<MatchEntry> = Vec::new();

    // Gather only the rules that could match this element via the index, instead of scanning
    // every rule in every sheet. We dedup per rule (keyed by its unique global `order`),
    // keeping the MAX specificity across that rule's matching selectors — exactly what the
    // brute-force `rule_specificity` (max over comma selectors) produced.
    //
    // `best_by_order` maps a rule's `order` to its (origin, max-specificity, decls). A rule's
    // origin and decls are constant for a given order, so the only thing we fold is the max
    // specificity.
    let mut best_by_order: HashMap<usize, (u8, u32, &[(String, String)], Option<&'a str>)> =
        HashMap::new();
    // Matching `::before`/`::after` rules, kept separately so they cascade onto the pseudo style
    // rather than the element itself. Each is (origin, specificity, order, decls, base).
    let mut before_matches: Vec<(u8, u32, usize, &'a [(String, String)], Option<&'a str>)> =
        Vec::new();
    let mut after_matches: Vec<(u8, u32, usize, &'a [(String, String)], Option<&'a str>)> =
        Vec::new();
    let mut consider = |entry: &Entry<'a>| {
        // The compound must match the originating element either way; the pseudo just routes the
        // declarations to the element's ::before/::after style.
        if !complex_matches(doc, node_id, &entry.compiled.selector) {
            return;
        }
        match &entry.compiled.pseudo_element {
            Some(PseudoElement::Before) => {
                before_matches.push((entry.origin, entry.compiled.specificity, entry.order, entry.decls, entry.base));
            }
            Some(PseudoElement::After) => {
                after_matches.push((entry.origin, entry.compiled.specificity, entry.order, entry.decls, entry.base));
            }
            // Other pseudo-elements (`::marker`, `::highlight(x)`, …) don't generate layout boxes
            // here and don't apply to the originating element; they're resolved on demand by
            // `compute_pseudo_style` for `getComputedStyle`.
            Some(PseudoElement::Other(_)) => {}
            None => {
                best_by_order
                    .entry(entry.order)
                    .and_modify(|(_, spec, _, _)| *spec = (*spec).max(entry.compiled.specificity))
                    .or_insert((entry.origin, entry.compiled.specificity, entry.decls, entry.base));
            }
        }
    };

    if let Some(id) = el.id() {
        if let Some(bucket) = index.by_id.get(id) {
            for e in bucket {
                consider(e);
            }
        }
    }
    for class in el.classes() {
        if let Some(bucket) = index.by_class.get(class) {
            for e in bucket {
                consider(e);
            }
        }
    }
    let tag_lower = el.tag.to_lowercase();
    if let Some(bucket) = index.by_type.get(&tag_lower) {
        for e in bucket {
            consider(e);
        }
    }
    for e in &index.universal {
        consider(e);
    }

    // Cascade-origin levels (sorted ascending, winner last): 0 = UA, 1 = presentational hints,
    // 2 = author, 3 = inline. The selector index tags UA entries 0 and author entries 1; remap
    // author to level 2 here so presentational hints (level 1) slot strictly between UA and author —
    // regardless of selector specificity (a UA `td { padding: 1px }` has specificity 1, but a hint
    // must still beat it for `cellpadding` to work, so origin level — not specificity — separates
    // them).
    for (order, (origin, specificity, decls, base)) in best_by_order {
        let level = if origin == 0 { 0 } else { 2 };
        matches.push(MatchEntry { origin: level, specificity, order, decls, base });
    }

    // Presentational hints: HTML attributes (`border`, `bgcolor`, `align`, `width`, …) mapped to
    // CSS declarations at origin level 1 — ABOVE the UA stylesheet, BELOW all author CSS. See
    // `presentational_hints`.
    let hint_decls: Vec<(String, String)> = presentational_hints(doc, node_id, el);
    if !hint_decls.is_empty() {
        matches.push(MatchEntry { origin: 1, specificity: 0, order: usize::MAX - 1, decls: &hint_decls, base: None });
    }

    // Inline style is its own origin (level 3) with highest precedence.
    let inline_decls: Vec<(String, String)> = el
        .attrs
        .get("style")
        .map(|s| css::parse_declarations(s))
        .unwrap_or_default();
    if !inline_decls.is_empty() {
        // Inline is the sole top-level entry; the sort tiebreaks on `order` only within the
        // same origin/specificity, so the exact value is immaterial. Use MAX to keep the
        // "applied last" intent explicit.
        // Inline `style=""` url()s resolve against the document base; the cascade doesn't carry it,
        // so leave `base: None` and let the engine resolve against the document URL as a fallback.
        matches.push(MatchEntry { origin: 3, specificity: 0, order: usize::MAX, decls: &inline_decls, base: None });
    }

    // Sort by (origin, specificity, order) ascending so the winner is applied last.
    matches.sort_by(|a, b| {
        a.origin
            .cmp(&b.origin)
            .then(a.specificity.cmp(&b.specificity))
            .then(a.order.cmp(&b.order))
    });

    // Build this element's custom-property environment: inherit the ancestors' vars, then
    // override with any `--name: value` declared on this element (in cascade order, so the
    // winning declaration applies last).
    let mut vars = parent_vars.clone();
    let mut declared_here: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in &matches {
        for (prop, val) in m.decls {
            if let Some(name) = prop.strip_prefix("--") {
                let key = format!("--{name}");
                declared_here.insert(key.clone());
                vars.insert(key, val.clone());
            }
        }
    }
    // Apply `@property` registrations to the custom-property environment. A registered
    // non-inherited property that this element did NOT declare resets to its initial value
    // (it does not inherit). Any registered property with an initial value that is still absent
    // is seeded with that initial value (so it appears in the computed-style enumeration on every
    // element). `*`-syntax registrations without an initial value are not seeded.
    REGISTERED_PROPERTIES.with(|c| {
        for pr in c.borrow().iter() {
            if !pr.inherits && !declared_here.contains(&pr.name) {
                if let Some(iv) = &pr.initial_value {
                    vars.insert(pr.name.clone(), iv.clone());
                } else {
                    // Non-inherited, no initial value: it must not carry an inherited value.
                    vars.remove(&pr.name);
                }
            } else if !vars.contains_key(&pr.name) {
                if let Some(iv) = &pr.initial_value {
                    vars.insert(pr.name.clone(), iv.clone());
                }
            }
        }
    });

    // Now apply the regular declarations, resolving any `var(...)` references against `vars`
    // and supplying the current/inherited color for `currentColor`/`inherit`.
    let inherited_color = parent.color;
    // `font-size` must be resolved before any other declaration, because `em`-based values
    // (insets, line-height, edges…) compute against *this element's* font size regardless of
    // declaration order. Apply the winning `font-size` first, then everything else.
    for m in &matches {
        for (prop, val) in m.decls {
            if prop.eq_ignore_ascii_case("font-size") {
                let (val, _imp) = split_importance(val);
                let resolved = resolve_vars(val, &vars);
                let current_color = style.color;
                apply_declaration(&mut style, prop, &resolved, parent, current_color, inherited_color, m.base);
            }
        }
    }
    // Normal (non-important) declarations, in ascending cascade order (later wins).
    for m in &matches {
        for (prop, val) in m.decls {
            if prop.starts_with("--") || prop.eq_ignore_ascii_case("font-size") {
                continue; // custom properties are environment; font-size already applied above
            }
            let (val, important) = split_importance(val);
            if important {
                continue; // important declarations are applied in the final pass below
            }
            let resolved = resolve_vars(val, &vars);
            let current_color = style.color;
            apply_declaration(&mut style, prop, &resolved, parent, current_color, inherited_color, m.base);
        }
    }
    // `!important` declarations win over all normal ones: apply them last, still in ascending
    // cascade order so the most-specific/last important declaration takes effect.
    for m in &matches {
        for (prop, val) in m.decls {
            if prop.starts_with("--") {
                continue;
            }
            let (val, important) = split_importance(val);
            if !important {
                continue;
            }
            let resolved = resolve_vars(val, &vars);
            let current_color = style.color;
            apply_declaration(&mut style, prop, &resolved, parent, current_color, inherited_color, m.base);
        }
    }

    // The UA stylesheet emits `display: block` for block tags; everything else defaults to
    // inline. If no author/UA rule set a display, fall back to the per-tag default.
    let display_was_set = matches.iter().any(|m| {
        m.decls.iter().any(|(p, _)| p.eq_ignore_ascii_case("display"))
    });
    if !display_was_set && style.display == Display::Inline && is_block_tag(&el.tag) {
        style.display = Display::Block;
    }
    if parent_hidden {
        style.display = Display::None;
    }

    // Keep the legacy derived flags in sync for existing readers (engine / layout fallbacks).
    style.display_none = style.display == Display::None;
    style.display_block = matches!(
        style.display,
        Display::Block | Display::Flex | Display::Grid | Display::None
    );

    // Cascade ::before / ::after styles. Each inherits from this element's computed style, then
    // applies its own matching rules. A pseudo-element only generates a box when its `content`
    // resolves to Some, so we keep the result only in that case.
    style.before = cascade_pseudo(&style, el, &before_matches, &vars);
    style.after = cascade_pseudo(&style, el, &after_matches, &vars);

    // Expose the resolved custom-property environment for CSSOM reads. Custom props inherit, so
    // this includes ancestor-declared vars (the *computed* value, per spec).
    style.custom_props = vars.clone();

    (style, vars)
}

/// Cascade a `::before`/`::after` pseudo-element's style from its originating element's computed
/// style (the inheritance source) plus the `matches` (origin, specificity, order, decls) rules
/// whose compound matched. Returns the boxed pseudo style only when `content` resolved to Some
/// (a pseudo with no `content` generates no box, per spec). `vars` is the element's custom-property
/// environment (pseudo-elements inherit it).
fn cascade_pseudo(
    element_style: &ComputedStyle,
    el: &dom::ElementData,
    matches: &[(u8, u32, usize, &[(String, String)], Option<&str>)],
    vars: &HashMap<String, String>,
) -> Option<Box<ComputedStyle>> {
    if matches.is_empty() {
        return None;
    }
    // Start from values inherited from the originating element (a fresh element-style snapshot,
    // already carrying the element's inherited typography/color), but reset the non-inherited
    // box/content fields to initial.
    let mut ps = element_style.clone();
    ps.background_color = None;
    ps.background_gradient = None;
    ps.mask_image = None;
    ps.box_shadows = Vec::new();
    ps.transform = None;
    ps.transform_origin = (0.5, 0.5);
    ps.margin = Edges::default();
    ps.padding = Edges::default();
    ps.border = Edges::default();
    ps.border_color = element_style.color;
    ps.width = None;
    ps.height = None;
    ps.min_width = None;
    ps.max_width = None;
    ps.min_height = None;
    ps.max_height = None;
    ps.position = Position::Static;
    ps.top = None;
    ps.right = None;
    ps.bottom = None;
    ps.left = None;
    ps.z_index = None;
    ps.opacity = 1.0;
    ps.border_radius = 0.0;
    ps.display = Display::Inline; // generated content is inline by default
    ps.display_block = false;
    ps.content = None;
    ps.before = None;
    ps.after = None;

    // Apply matching rules in cascade order (origin, specificity, source order ascending → winner
    // last). The inheritance source for `currentColor`/`inherit` is the originating element.
    let mut sorted: Vec<&(u8, u32, usize, &[(String, String)], Option<&str>)> =
        matches.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let inherited_color = element_style.color;
    for (_, _, _, decls, base) in sorted {
        for (prop, val) in *decls {
            if prop.starts_with("--") {
                continue;
            }
            let resolved = resolve_vars(val, vars);
            let current_color = ps.color;
            apply_declaration(&mut ps, prop, &resolved, element_style, current_color, inherited_color, *base);
        }
    }

    // No `content` (or `content: none`) → no generated box.
    let content = ps.content.take()?;
    // Resolve `attr(name)` now that we have the element.
    ps.content = Some(resolve_content_attr(&content, el));
    // Keep derived display flags consistent for downstream readers.
    ps.display_none = ps.display == Display::None;
    ps.display_block = matches!(
        ps.display,
        Display::Block | Display::Flex | Display::Grid | Display::None
    );
    Some(Box::new(ps))
}

/// Gather the candidate index entries for `el` (its id/class/type buckets + the universal bucket),
/// the same set `cascade_node` considers. Returned in bucket order; callers filter + sort.
fn candidate_entries<'i, 'a>(index: &'i SelectorIndex<'a>, el: &dom::ElementData) -> Vec<&'i Entry<'a>> {
    let mut out: Vec<&'i Entry<'a>> = Vec::new();
    if let Some(id) = el.id() {
        if let Some(bucket) = index.by_id.get(id) {
            out.extend(bucket.iter());
        }
    }
    for class in el.classes() {
        if let Some(bucket) = index.by_class.get(class) {
            out.extend(bucket.iter());
        }
    }
    let tag_lower = el.tag.to_lowercase();
    if let Some(bucket) = index.by_type.get(&tag_lower) {
        out.extend(bucket.iter());
    }
    out.extend(index.universal.iter());
    out
}

/// Compute the cascaded computed style of a pseudo-element of `node_id`, for `getComputedStyle`.
///
/// `element_style` is the originating element's already-cascaded style (the inheritance source).
/// `pseudo_key` is the canonical key from [`parse_gcs_pseudo`] (`"before"`, `"marker"`,
/// `"highlight(x)"`, …). Returns `None` only if `node_id` isn't an element; otherwise it always
/// returns a (possibly rule-less, but non-empty) pseudo style — matching browsers, which expose a
/// full computed style for any tree-abiding pseudo-element of any element.
///
/// Box / non-inherited properties start at their initial values (the pseudo is a fresh box that
/// merely inherits typography/color from the originating element); matching author + UA rules for
/// that pseudo then cascade on top. `content` is *not* required (unlike layout box generation) —
/// `getComputedStyle(el, "::before")` reports a style even when there's no generated box.
pub fn compute_pseudo_style(
    doc: &dom::Document,
    sheets: &[css::Stylesheet],
    node_id: dom::NodeId,
    element_style: &ComputedStyle,
    pseudo_key: &str,
) -> Option<ComputedStyle> {
    let el = el_of(doc, node_id)?.clone();
    let ua = user_agent_stylesheet();
    let index = SelectorIndex::build(&ua, sheets);

    // Collect every rule whose compound matches the originating element AND whose pseudo-element
    // equals the requested key. Mirror `cascade_node`'s bucketed lookup.
    let mut matches: Vec<(u8, u32, usize, &[(String, String)], Option<&str>)> = Vec::new();
    for entry in candidate_entries(&index, &el) {
        if !matches!(&entry.compiled.pseudo_element, Some(pe) if pe.key() == pseudo_key) {
            continue;
        }
        if !complex_matches(doc, node_id, &entry.compiled.selector) {
            continue;
        }
        let origin = if entry.origin == 0 { 0 } else { 2 };
        matches.push((origin, entry.compiled.specificity, entry.order, entry.decls, entry.base));
    }

    // Inherit typography/color from the originating element, then reset the box / non-inherited
    // fields to their initial values (same reset list as `cascade_pseudo`).
    let mut ps = element_style.clone();
    ps.background_color = None;
    ps.background_gradient = None;
    ps.mask_image = None;
    ps.box_shadows = Vec::new();
    ps.transform = None;
    ps.transform_origin = (0.5, 0.5);
    ps.margin = Edges::default();
    ps.padding = Edges::default();
    ps.border = Edges::default();
    ps.border_color = element_style.color;
    ps.width = None;
    ps.height = None;
    ps.min_width = None;
    ps.max_width = None;
    ps.min_height = None;
    ps.max_height = None;
    ps.position = Position::Static;
    ps.top = None;
    ps.right = None;
    ps.bottom = None;
    ps.left = None;
    ps.z_index = None;
    ps.opacity = 1.0;
    ps.border_radius = 0.0;
    ps.display = Display::Inline; // generated content is inline by default
    ps.display_block = false;
    ps.display_none = false;
    ps.content = None;
    ps.before = None;
    ps.after = None;

    // The originating element's custom-property environment is inherited by its pseudos. Rebuild it
    // here from the element's matching declarations (the cascade doesn't expose the stored map).
    let vars = element_vars(doc, node_id, &el, &index);

    matches.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let inherited_color = element_style.color;
    // `parse_length` drops percentage width/height (it has no basis at cascade time). For a
    // pseudo-element the containing block IS the originating element's box, which we know here, so
    // track the winning percentage and resolve it against the element's content extents.
    let mut width_pct: Option<f32> = None;
    let mut height_pct: Option<f32> = None;
    for (_, _, _, decls, base) in &matches {
        for (prop, val) in *decls {
            if prop.starts_with("--") {
                continue;
            }
            let resolved = resolve_vars(val, &vars);
            let current_color = ps.color;
            match prop.as_str() {
                "width" => width_pct = parse_percent(&resolved),
                "height" => height_pct = parse_percent(&resolved),
                _ => {}
            }
            apply_declaration(&mut ps, prop, &resolved, element_style, current_color, inherited_color, *base);
        }
    }
    // Resolve a tracked percentage width/height against the originating element's content box. Only
    // when `apply_declaration` left the field as `None` (i.e. it was a percentage it couldn't store).
    if let Some(p) = width_pct {
        if ps.width.is_none() {
            if let Some(basis) = element_style.width {
                ps.width = Some(p / 100.0 * basis);
            }
        }
    }
    if let Some(p) = height_pct {
        if ps.height.is_none() {
            if let Some(basis) = element_style.height {
                ps.height = Some(p / 100.0 * basis);
            }
        }
    }

    // Resolve `content`'s `attr()` now that we have the element (if any content was set).
    if let Some(content) = ps.content.take() {
        ps.content = Some(resolve_content_attr(&content, &el));
    }

    // Item-based blockification: a pseudo-element child of a flex/grid container is blockified.
    if matches!(element_style.display, Display::Flex | Display::Grid)
        && matches!(ps.display, Display::Inline)
    {
        ps.display = Display::Block;
    }

    // Keep derived display flags consistent for downstream readers.
    ps.display_none = ps.display == Display::None;
    ps.display_block = matches!(
        ps.display,
        Display::Block | Display::Flex | Display::Grid | Display::None
    );
    Some(ps)
}

/// Rebuild an element's custom-property (`--name`) environment by cascading its own matching
/// declarations. Used to give a pseudo-element the same `var()` environment as its originating
/// element (the cascade keeps the map internally and doesn't expose it on `ComputedStyle`).
fn element_vars(
    doc: &dom::Document,
    node_id: dom::NodeId,
    el: &dom::ElementData,
    index: &SelectorIndex,
) -> HashMap<String, String> {
    let mut entries: Vec<(u8, u32, usize, &[(String, String)])> = Vec::new();
    for entry in candidate_entries(index, el) {
        if entry.compiled.pseudo_element.is_some() {
            continue;
        }
        if !complex_matches(doc, node_id, &entry.compiled.selector) {
            continue;
        }
        let origin = if entry.origin == 0 { 0 } else { 2 };
        entries.push((origin, entry.compiled.specificity, entry.order, entry.decls));
    }
    // Inline style vars too.
    let inline_decls: Vec<(String, String)> = el
        .attrs
        .get("style")
        .map(|s| css::parse_declarations(s))
        .unwrap_or_default();

    entries.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let mut vars = HashMap::new();
    for (_, _, _, decls) in &entries {
        for (prop, val) in *decls {
            if let Some(name) = prop.strip_prefix("--") {
                vars.insert(format!("--{name}"), val.clone());
            }
        }
    }
    for (prop, val) in &inline_decls {
        if let Some(name) = prop.strip_prefix("--") {
            vars.insert(format!("--{name}"), val.clone());
        }
    }
    vars
}

/// Resolve `var(--name, fallback)` references in `value` against `vars`, recursively (vars can
/// reference vars). Bounded against cyclic references by a recursion-depth cap.
fn resolve_vars(value: &str, vars: &HashMap<String, String>) -> String {
    resolve_vars_depth(value, vars, 0)
}

const VAR_MAX_DEPTH: usize = 32;

fn resolve_vars_depth(value: &str, vars: &HashMap<String, String>, depth: usize) -> String {
    if depth >= VAR_MAX_DEPTH || !value.contains("var(") {
        return value.to_string();
    }
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < chars.len() {
        // Detect `var(` at a token boundary.
        if chars[i] == 'v'
            && chars[i..].len() >= 4
            && chars[i + 1] == 'a'
            && chars[i + 2] == 'r'
            && chars[i + 3] == '('
        {
            // Find the matching close paren for this `var(`.
            let args_start = i + 4;
            let mut j = args_start;
            let mut pdepth = 1i32;
            while j < chars.len() && pdepth > 0 {
                match chars[j] {
                    '(' => pdepth += 1,
                    ')' => pdepth -= 1,
                    _ => {}
                }
                if pdepth == 0 {
                    break;
                }
                j += 1;
            }
            // chars[j] is the matching ')'.
            let args: String = chars[args_start..j].iter().collect();
            let replacement = resolve_one_var(&args, vars, depth);
            out.push_str(&replacement);
            i = j + 1; // skip past ')'
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Resolve the args of a single `var(...)`: `--name` or `--name, fallback`. Returns the
/// resolved (and recursively var-expanded) value, or the (expanded) fallback, or empty.
fn resolve_one_var(args: &str, vars: &HashMap<String, String>, depth: usize) -> String {
    // Split into name and optional fallback at the first top-level comma.
    let (name, fallback) = split_first_comma(args);
    let name = name.trim();
    if let Some(v) = vars.get(name) {
        // The looked-up value may itself contain var() references.
        return resolve_vars_depth(v, vars, depth + 1);
    }
    match fallback {
        Some(fb) => resolve_vars_depth(fb.trim(), vars, depth + 1),
        None => String::new(),
    }
}

/// Split `s` at the first top-level comma (not inside nested parens). Returns `(before, after)`.
fn split_first_comma(s: &str) -> (&str, Option<&str>) {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    for (idx, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => return (&s[..idx], Some(&s[idx + 1..])),
            _ => {}
        }
    }
    (s, None)
}

/// Decide whether a rule with the given raw `@media` query applies at the assumed desktop
/// viewport ([`assumed_viewport_width()`]). `None` (no media) always applies. We parse the
/// common Tailwind shapes: `screen`/`all` match, `print` does not, and single
/// `min-width`/`max-width` px thresholds are compared against the assumed width. Multiple
/// `and`-joined conditions must all pass. Unrecognized features are treated as matching
/// (best-effort, so we don't drop rules we can't fully parse).
fn media_applies(media: Option<&str>) -> bool {
    let query = match media {
        None => return true,
        Some(q) => q.trim(),
    };
    if query.is_empty() {
        return true;
    }
    // A comma-separated media query list matches if ANY component matches.
    query.split(',').any(|component| media_component_matches(component))
}

fn media_component_matches(component: &str) -> bool {
    let lower = component.trim().to_ascii_lowercase();
    // Split on `and`; each part is a media type or a `(feature: value)` condition.
    for raw in lower.split(" and ") {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        // Media types.
        if part == "screen" || part == "all" {
            continue;
        }
        if part == "print" {
            return false;
        }
        // Feature conditions like `(min-width: 768px)`.
        if let Some(inner) = part.strip_prefix('(').and_then(|p| p.strip_suffix(')')) {
            if let Some((feature, value)) = inner.split_once(':') {
                let feature = feature.trim();
                let value = value.trim();
                match feature {
                    "min-width" => {
                        if let Some(px) = length_px(value) {
                            if assumed_viewport_width() < px {
                                return false;
                            }
                        }
                    }
                    "max-width" => {
                        if let Some(px) = length_px(value) {
                            if assumed_viewport_width() > px {
                                return false;
                            }
                        }
                    }
                    "min-height" => {
                        if let Some(px) = length_px(value) {
                            if assumed_viewport_height() < px {
                                return false;
                            }
                        }
                    }
                    "max-height" => {
                        if let Some(px) = length_px(value) {
                            if assumed_viewport_height() > px {
                                return false;
                            }
                        }
                    }
                    // Resolution / HiDPI queries, compared against the real device pixel ratio.
                    "min-resolution" | "-webkit-min-device-pixel-ratio" | "min--moz-device-pixel-ratio" => {
                        if let Some(r) = resolution_dppx(value) {
                            if viewport_dpr() < r {
                                return false;
                            }
                        }
                    }
                    "max-resolution" | "-webkit-max-device-pixel-ratio" | "max--moz-device-pixel-ratio" => {
                        if let Some(r) = resolution_dppx(value) {
                            if viewport_dpr() > r {
                                return false;
                            }
                        }
                    }
                    "orientation" => {
                        let landscape = assumed_viewport_width() >= assumed_viewport_height();
                        if (value == "portrait" && landscape) || (value == "landscape" && !landscape) {
                            return false;
                        }
                    }
                    // Real OS appearance: `dark` rules apply only in Dark mode, `light` only in
                    // Light. This is what actually restyles most dark-mode-aware sites.
                    "prefers-color-scheme" => {
                        let dark = color_scheme_dark();
                        if (value == "dark" && !dark) || (value == "light" && dark) {
                            return false;
                        }
                    }
                    // Unrecognized features (other prefers-*, hover, …): treat as matching.
                    _ => {}
                }
            }
            continue;
        }
        // Bare `not`/`only` prefixes or unknown tokens: be permissive (treat as matching),
        // except an explicit leading `not` which we honor crudely.
        if part.starts_with("not ") {
            return false;
        }
    }
    true
}

/// Decide whether a rule with the given raw `@container` condition applies, evaluated against an
/// assumed container width ([`assumed_container_width()`]). Correct container sizing needs layout
/// (which runs after the cascade), so this is a pragmatic approximation that mirrors
/// [`media_applies`]: `min-width`/`max-width`/`inline-size`/`width` thresholds are compared
/// against the assumed width; multiple `and`-joined conditions must all pass. `None` (no
/// container) always applies, and unrecognized conditions are treated permissively (applied) so
/// container rules aren't dropped.
fn container_applies(container: Option<&str>) -> bool {
    let query = match container {
        None => return true,
        Some(q) => q.trim(),
    };
    if query.is_empty() {
        return true;
    }
    // Conditions joined by `and` must all match. We also tolerate a `(width > 400px)`-style
    // comparison form in addition to the `(min-width: 400px)` colon form.
    let lower = query.to_ascii_lowercase();
    for raw in lower.split(" and ") {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(inner) = part.strip_prefix('(').and_then(|p| p.strip_suffix(')')) {
            if !container_feature_matches(inner.trim()) {
                return false;
            }
        }
        // Non-parenthesized tokens (a bare container name etc.) are ignored → permissive.
    }
    true
}

/// Evaluate a single `@container` feature condition (the text inside the parens) against
/// [`assumed_container_width()`]. Handles the colon form (`min-width: 400px`,
/// `max-inline-size: 600px`) and the range form (`width >= 400px`, `inline-size < 600px`).
/// Unrecognized features/forms → `true` (permissive).
fn container_feature_matches(inner: &str) -> bool {
    let w = assumed_container_width();
    // Colon form: `feature: value`.
    if let Some((feature, value)) = inner.split_once(':') {
        let feature = feature.trim();
        let value = value.trim();
        if let Some(px) = length_px(value) {
            return match feature {
                "min-width" | "min-inline-size" => w >= px,
                "max-width" | "max-inline-size" => w <= px,
                _ => true, // height/aspect/orientation/unknown → permissive
            };
        }
        return true;
    }
    // Range form: `feature OP value` where OP is one of >= <= > < =.
    for (op, less, oreq) in [(">=", false, true), ("<=", true, true), (">", false, false), ("<", true, false), ("=", false, false)] {
        if let Some((feature, value)) = inner.split_once(op) {
            let feature = feature.trim();
            if !matches!(feature, "width" | "inline-size" | "height" | "block-size") {
                return true; // unknown feature → permissive
            }
            if matches!(feature, "height" | "block-size") {
                return true; // no assumed container height → permissive
            }
            if let Some(px) = length_px(value.trim()) {
                return match (less, oreq) {
                    (false, true) => w >= px,
                    (true, true) => w <= px,
                    (false, false) if op == "=" => (w - px).abs() < f32::EPSILON,
                    (false, false) => w > px,
                    (true, false) => w < px,
                };
            }
            return true;
        }
    }
    true // unrecognized form → permissive
}

/// Parse a media-query length to px. Supports `px`, `rem`/`em` (×16), bare numbers (px).
fn length_px(value: &str) -> Option<f32> {
    let v = value.trim().to_ascii_lowercase();
    if let Some(n) = v.strip_suffix("px") {
        n.trim().parse::<f32>().ok()
    } else if let Some(n) = v.strip_suffix("rem") {
        n.trim().parse::<f32>().ok().map(|x| x * 16.0)
    } else if let Some(n) = v.strip_suffix("em") {
        n.trim().parse::<f32>().ok().map(|x| x * 16.0)
    } else {
        v.parse::<f32>().ok()
    }
}

/// Parse a resolution value into dppx (dots per `px`, i.e. the device pixel ratio): `2dppx`/`2x`
/// → 2, `192dpi` → 2 (96dpi = 1dppx), `96dpcm`→…, or a bare number (the `-webkit-*-device-pixel-ratio`
/// form) → that number.
fn resolution_dppx(value: &str) -> Option<f32> {
    let v = value.trim().to_ascii_lowercase();
    if let Some(n) = v.strip_suffix("dppx").or_else(|| v.strip_suffix('x')) {
        n.trim().parse::<f32>().ok()
    } else if let Some(n) = v.strip_suffix("dpi") {
        n.trim().parse::<f32>().ok().map(|x| x / 96.0)
    } else if let Some(n) = v.strip_suffix("dpcm") {
        n.trim().parse::<f32>().ok().map(|x| x / 96.0 * 2.54)
    } else {
        v.parse::<f32>().ok()
    }
}

/// Block-level-by-default tags (mirrors the layout UA list).
fn is_block_tag(tag: &str) -> bool {
    matches!(
        tag.to_ascii_lowercase().as_str(),
        "html" | "body" | "div" | "p" | "section" | "article" | "header" | "footer" | "nav"
            | "main" | "aside" | "ul" | "ol" | "li" | "blockquote" | "pre" | "table" | "tr"
            | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "form" | "fieldset" | "figure"
            | "figcaption" | "address" | "hr"
    )
}

/// Parse a `justify-content` / `align-content` keyword.
fn parse_justify(val: &str) -> Option<JustifyContent> {
    match val.trim().to_ascii_lowercase().as_str() {
        "flex-start" | "start" | "left" => Some(JustifyContent::FlexStart),
        "flex-end" | "end" | "right" => Some(JustifyContent::FlexEnd),
        "center" => Some(JustifyContent::Center),
        "space-between" => Some(JustifyContent::SpaceBetween),
        "space-around" => Some(JustifyContent::SpaceAround),
        "space-evenly" => Some(JustifyContent::SpaceEvenly),
        _ => None,
    }
}

/// Parse the `flex` shorthand. Supported forms:
/// - `none` → grow 0, shrink 0, basis auto
/// - `auto` → grow 1, shrink 1, basis auto
/// - a single number `N` → grow N, shrink 1, basis 0
/// - `grow shrink basis` (2 or 3 tokens; unitless numbers are grow/shrink, a length is basis)
fn apply_flex_shorthand(style: &mut ComputedStyle, val: &str) {
    let v = val.trim().to_ascii_lowercase();
    if v == "none" {
        style.flex_grow = 0.0;
        style.flex_shrink = 0.0;
        style.flex_basis = None;
        return;
    }
    if v == "auto" {
        style.flex_grow = 1.0;
        style.flex_shrink = 1.0;
        style.flex_basis = None;
        return;
    }
    if v == "initial" {
        style.flex_grow = 0.0;
        style.flex_shrink = 1.0;
        style.flex_basis = None;
        return;
    }

    let toks: Vec<&str> = val.split_whitespace().collect();
    // Classify tokens: unitless numbers (no %/px/unit) are grow then shrink; anything that
    // parses as a length (or `auto`/0px) is the basis.
    let mut nums: Vec<f32> = Vec::new();
    let mut basis: Option<Option<f32>> = None; // Some(None)=auto, Some(Some(x))=px
    for t in &toks {
        let tl = t.to_ascii_lowercase();
        if tl == "auto" {
            basis = Some(None);
            continue;
        }
        // A bare unitless integer/float is a flex factor; a value with a unit/% is the basis.
        let has_unit = tl.ends_with("px")
            || tl.ends_with('%')
            || tl.ends_with("pt")
            || tl.ends_with("em")
            || tl.ends_with("rem");
        if !has_unit {
            if let Ok(n) = tl.parse::<f32>() {
                if nums.len() < 2 {
                    nums.push(n);
                    continue;
                }
            }
        }
        // Otherwise treat as a length basis.
        basis = Some(parse_length(t));
    }
    match nums.len() {
        0 => {}
        1 => {
            style.flex_grow = nums[0].max(0.0);
            style.flex_shrink = 1.0;
            // `flex: 1` → basis 0 unless an explicit basis was given.
            if basis.is_none() {
                style.flex_basis = Some(0.0);
            }
        }
        _ => {
            style.flex_grow = nums[0].max(0.0);
            style.flex_shrink = nums[1].max(0.0);
            if basis.is_none() {
                style.flex_basis = Some(0.0);
            }
        }
    }
    if let Some(b) = basis {
        style.flex_basis = b;
    }
}

/// Parse a `gap` value: 1 value → both row & column; 2 values → row column.
fn parse_gap(val: &str) -> Option<(f32, f32)> {
    let parts: Vec<f32> = val
        .split_whitespace()
        .filter_map(parse_length)
        .collect();
    match parts.len() {
        1 => Some((parts[0], parts[0])),
        n if n >= 2 => Some((parts[0], parts[1])),
        _ => None,
    }
}

/// Parse a space-separated grid track list. Supports `Npx`, `Nfr`, `N%`, `auto`, and
/// `repeat(n, <track>)` (expanded). Unrecognized tokens are skipped.
fn parse_track_list(val: &str) -> Vec<TrackSize> {
    let mut out = Vec::new();
    let lower = val.trim().to_ascii_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        // Read a token up to whitespace, but keep balanced parens together (for repeat()).
        let start = i;
        let mut depth = 0i32;
        while i < chars.len() {
            let c = chars[i];
            if c == '(' {
                depth += 1;
            } else if c == ')' {
                depth -= 1;
            } else if c.is_whitespace() && depth == 0 {
                break;
            }
            i += 1;
        }
        let tok: String = chars[start..i].iter().collect();
        if let Some(inner) = tok.strip_prefix("repeat(").and_then(|s| s.strip_suffix(')')) {
            // repeat(count, tracks...)
            if let Some((count_s, rest)) = inner.split_once(',') {
                if let Ok(count) = count_s.trim().parse::<usize>() {
                    let inner_tracks = parse_track_list(rest);
                    for _ in 0..count.min(1000) {
                        out.extend(inner_tracks.iter().copied());
                    }
                }
            }
        } else if let Some(t) = parse_track_size(&tok) {
            out.push(t);
        }
    }
    out
}

/// Parse a single grid track size token.
fn parse_track_size(tok: &str) -> Option<TrackSize> {
    let t = tok.trim();
    if t == "auto" {
        return Some(TrackSize::Auto);
    }
    if let Some(fr) = t.strip_suffix("fr") {
        return fr.trim().parse::<f32>().ok().map(TrackSize::Fr);
    }
    if let Some(pct) = t.strip_suffix('%') {
        return pct.trim().parse::<f32>().ok().map(TrackSize::Pct);
    }
    parse_length(t).map(TrackSize::Px)
}

/// Parse a `grid-column` / `grid-row` placement: `a`, `a / b`, `a / span N`, or `span N`.
fn parse_grid_placement(val: &str) -> Option<GridPlacement> {
    let v = val.trim().to_ascii_lowercase();
    if v.is_empty() || v == "auto" {
        return None;
    }
    let (start_s, end_s) = match v.split_once('/') {
        Some((a, b)) => (a.trim(), Some(b.trim())),
        None => (v.as_str(), None),
    };

    // Parse the start side. It may itself be a `span N`.
    let (start, span_from_start) = if let Some(n) = start_s.strip_prefix("span") {
        (None, n.trim().parse::<i32>().ok())
    } else {
        (start_s.parse::<i32>().ok(), None)
    };

    let end = match end_s {
        None => {
            if let Some(s) = span_from_start {
                GridEnd::Span(s)
            } else {
                GridEnd::Auto
            }
        }
        Some(e) => {
            if let Some(n) = e.strip_prefix("span") {
                n.trim().parse::<i32>().ok().map(GridEnd::Span).unwrap_or(GridEnd::Auto)
            } else if let Ok(line) = e.parse::<i32>() {
                GridEnd::Line(line)
            } else {
                GridEnd::Auto
            }
        }
    };

    if start.is_none() && matches!(end, GridEnd::Auto) {
        return None;
    }
    Some(GridPlacement { start, end })
}

/// Map legacy HTML presentational attributes to CSS declarations ("presentational hints", per the
/// HTML spec). The returned declarations are injected into the cascade at the very end of the UA
/// origin — above the UA stylesheet, below all author CSS — so author rules always win while these
/// still override UA defaults (e.g. `cellpadding` beating `td { padding: 1px }`).
///
/// Honored:
/// - `<table border="N">` → `border: Npx solid` on the table AND `border: 1px solid` on every
///   descendant `<td>`/`<th>` (resolved by walking up to the nearest ancestor `<table>`).
/// - `<table cellspacing="N">` → `border-spacing: Npx`; `cellpadding="N">` → `padding: Npx` on each
///   cell (again resolved from the nearest ancestor `<table>`).
/// - `bgcolor` on `table`/`tr`/`td`/`th`/`body` → `background-color` (named, `#rgb`, `#rrggbb`).
/// - `align=left|center|right` on `td`/`th`/`tr` → `text-align`. On `table`/`img` it is skipped
///   (float/box alignment isn't modeled) — documented as a gap.
/// - `valign` on `td`/`th`/`tr` → `vertical-align` (the value is mapped; layout only honors `top`).
/// - `width`/`height` (`N` px or `N%`) on `table`/`td`/`th` → `width`/`height` (px only; `%` is
///   mapped to a `%` string which the length parser drops — a documented gap for table/cell `%`).
///   `<img>` width/height are handled in the replaced-element path, so they are NOT emitted here.
/// - `<font color>` → `color`; `<font size>` is skipped (the legacy 1–7 scale is awkward) — gap.
fn presentational_hints(
    doc: &dom::Document,
    node_id: dom::NodeId,
    el: &dom::ElementData,
) -> Vec<(String, String)> {
    let tag = el.tag.to_ascii_lowercase();
    let mut out: Vec<(String, String)> = Vec::new();
    let attr = |name: &str| el.attrs.get(name).map(|s| s.trim().to_string());

    // A length attribute value: bare number → px; `N%` → percent string (length parser ignores %).
    let len_to_css = |v: &str| -> String {
        let t = v.trim();
        if let Some(p) = t.strip_suffix('%') {
            if p.trim().parse::<f32>().is_ok() {
                return format!("{}%", p.trim());
            }
        }
        // strip a trailing "px" if the author wrote one, else treat the number as px.
        let n = t.trim_end_matches("px").trim();
        if n.parse::<f32>().is_ok() { format!("{n}px") } else { t.to_string() }
    };

    match tag.as_str() {
        "table" => {
            if let Some(b) = attr("border") {
                // `border` (even `border=""` / `border="1"`) → a solid border of N px on the table.
                let n: f32 = b.parse().unwrap_or(1.0);
                if n > 0.0 || b.is_empty() {
                    let w = if b.is_empty() { 1.0 } else { n };
                    out.push(("border".into(), format!("{w}px solid")));
                }
            }
            if let Some(s) = attr("cellspacing") {
                if let Ok(n) = s.trim_end_matches("px").trim().parse::<f32>() {
                    out.push(("border-spacing".into(), format!("{n}px")));
                }
            }
            if let Some(c) = attr("bgcolor") {
                out.push(("background-color".into(), c));
            }
            if let Some(w) = attr("width") {
                out.push(("width".into(), len_to_css(&w)));
            }
            if let Some(h) = attr("height") {
                out.push(("height".into(), len_to_css(&h)));
            }
        }
        "td" | "th" => {
            // Cell-level: inherit `border`/`cellpadding` from the nearest ancestor `<table>`.
            if let Some(tbl) = ancestor_table(doc, node_id) {
                if let Some(b) = tbl.attrs.get("border") {
                    let n: f32 = b.trim().parse().unwrap_or(1.0);
                    if n > 0.0 || b.trim().is_empty() {
                        // Per HTML rules, any non-zero table `border` puts a 1px border on cells.
                        out.push(("border".into(), "1px solid".into()));
                    }
                }
                if let Some(p) = tbl.attrs.get("cellpadding") {
                    if let Ok(n) = p.trim().trim_end_matches("px").trim().parse::<f32>() {
                        out.push(("padding".into(), format!("{n}px")));
                    }
                }
            }
            if let Some(a) = attr("align").map(|a| a.to_ascii_lowercase()) {
                if matches!(a.as_str(), "left" | "center" | "right") {
                    out.push(("text-align".into(), a));
                }
            }
            if let Some(v) = attr("valign").map(|v| v.to_ascii_lowercase()) {
                out.push(("vertical-align".into(), v));
            }
            if let Some(c) = attr("bgcolor") {
                out.push(("background-color".into(), c));
            }
            if let Some(w) = attr("width") {
                out.push(("width".into(), len_to_css(&w)));
            }
            if let Some(h) = attr("height") {
                out.push(("height".into(), len_to_css(&h)));
            }
        }
        "col" | "colgroup" => {
            // `<col width="N">` / `<colgroup width="N">` → a column width (consumed by `layout_table`
            // via the column's computed `width`). `span` is read directly off the attribute there.
            if let Some(w) = attr("width") {
                out.push(("width".into(), len_to_css(&w)));
            }
        }
        "tr" => {
            if let Some(c) = attr("bgcolor") {
                out.push(("background-color".into(), c));
            }
            if let Some(a) = attr("align").map(|a| a.to_ascii_lowercase()) {
                if matches!(a.as_str(), "left" | "center" | "right") {
                    out.push(("text-align".into(), a));
                }
            }
            if let Some(v) = attr("valign").map(|v| v.to_ascii_lowercase()) {
                out.push(("vertical-align".into(), v));
            }
        }
        "body" => {
            if let Some(c) = attr("bgcolor") {
                out.push(("background-color".into(), c));
            }
            if let Some(c) = attr("text") {
                out.push(("color".into(), c));
            }
        }
        "font" => {
            if let Some(c) = attr("color") {
                out.push(("color".into(), c));
            }
            // `size` (the legacy 1..7 / relative scale) is intentionally skipped.
        }
        _ => {}
    }
    out
}

/// Walk up from `node_id` to the nearest ancestor `<table>` element, returning its element data
/// (used to read `border`/`cellpadding` for a descendant cell's presentational hints).
fn ancestor_table(doc: &dom::Document, node_id: dom::NodeId) -> Option<&dom::ElementData> {
    let mut cur = doc.get(node_id).parent;
    while let Some(id) = cur {
        let node = doc.get(id);
        if let dom::NodeData::Element(el) = &node.data {
            if el.tag.eq_ignore_ascii_case("table") {
                return Some(el);
            }
        }
        cur = node.parent;
    }
    None
}

/// Apply a single declaration to `style`. Unknown properties/values are ignored silently.
#[allow(clippy::too_many_arguments)]
fn apply_declaration(
    style: &mut ComputedStyle,
    prop: &str,
    val: &str,
    parent: &ComputedStyle,
    current_color: (u8, u8, u8),
    inherited_color: (u8, u8, u8),
    base: Option<&str>,
) {
    match prop {
        "color" => {
            let trimmed = val.trim().to_ascii_lowercase();
            if trimmed == "inherit" {
                style.color = inherited_color;
            } else if trimmed == "initial" || trimmed == "unset" {
                style.color = ComputedStyle::default().color;
            } else if let Some(c) = parse_color_ctx(val, current_color, inherited_color) {
                style.color = c;
            }
        }
        "background-color" | "background" => {
            // First try a gradient (works for the `background` shorthand and `background-image`).
            if let Some(g) = parse_gradient(val, current_color, inherited_color) {
                style.background_gradient = Some(g);
            } else if let Some(c) = parse_color_ctx(val, current_color, inherited_color) {
                // Solid color interpretation; `transparent`/`none` leave it unchanged.
                style.background_color = Some(c);
            }
        }
        "background-image" => {
            if let Some(g) = parse_gradient(val, current_color, inherited_color) {
                style.background_gradient = Some(g);
            } else if val.trim().eq_ignore_ascii_case("none") {
                style.background_gradient = None;
            }
        }
        "color-scheme" => {
            let trimmed = val.trim().to_ascii_lowercase();
            if trimmed == "inherit" {
                style.color_scheme = parent.color_scheme;
            } else if let Some(cs) = parse_color_scheme(&trimmed) {
                style.color_scheme = cs;
            }
        }
        // `mask` / `mask-image` and the WebKit-prefixed aliases. The icon technique:
        // `background: currentColor; mask: url(icon.svg) no-repeat center / contain`. We parse past
        // `no-repeat` / position / `/ size`, extracting the `url(...)` source (already `var()`-
        // resolved by the caller) plus the contain/cover size keyword. `none` clears the mask.
        "mask" | "mask-image" | "-webkit-mask" | "-webkit-mask-image" => {
            let v = val.trim();
            if v.eq_ignore_ascii_case("none") {
                style.mask_image = None;
            } else if let Some(mut m) = parse_mask(v) {
                // Resolve the (post-`var()`) relative `url(...)` against the *stylesheet's* own base
                // URL (per CSS), so it's absolute by the time the engine fetches it. `data:` URLs
                // and already-absolute URLs pass through unchanged; with no base the engine resolves
                // it against the document URL as a fallback.
                m.url = resolve_css_url(&m.url, base);
                style.mask_image = Some(m);
            }
        }
        "box-shadow" => {
            let shadows = parse_box_shadows(val, current_color, inherited_color);
            if val.trim().eq_ignore_ascii_case("none") {
                style.box_shadows.clear();
            } else if !shadows.is_empty() {
                style.box_shadows = shadows;
            }
        }
        "transform" => {
            let v = val.trim();
            if v.eq_ignore_ascii_case("none") {
                style.transform = None;
            } else if let Some(m) = parse_transform(v) {
                style.transform = Some(m);
            }
        }
        "transform-origin" => {
            style.transform_origin = parse_transform_origin(val);
        }
        "font-size" => {
            if let Some(sz) = parse_font_size(val, parent.font_size) {
                style.font_size = sz;
            }
        }
        "font-family" => {
            let trimmed = val.trim();
            let lower = trimmed.to_ascii_lowercase();
            if lower == "inherit" {
                style.font_family = parent.font_family.clone();
            } else if lower == "initial" || lower == "unset" {
                // font-family inherits, so `unset` == `inherit`; `initial` is the UA default (None).
                style.font_family = if lower == "unset" { parent.font_family.clone() } else { None };
            } else if !trimmed.is_empty() {
                style.font_family = Some(serialize_font_family(trimmed));
            }
        }
        "font-weight" => match parse_font_weight(val) {
            Some(b) => style.bold = b,
            None => {}
        },
        "font-style" => match val.trim().to_ascii_lowercase().as_str() {
            "italic" | "oblique" => style.italic = true,
            "normal" => style.italic = false,
            _ => {}
        },
        "text-align" => match val.trim().to_ascii_lowercase().as_str() {
            "left" => style.text_align = TextAlign::Left,
            "center" => style.text_align = TextAlign::Center,
            "right" => style.text_align = TextAlign::Right,
            _ => {}
        },
        "vertical-align" => match val.trim().to_ascii_lowercase().as_str() {
            "sub" => style.vertical_align = VerticalAlign::Sub,
            "super" => style.vertical_align = VerticalAlign::Super,
            "baseline" => style.vertical_align = VerticalAlign::Baseline,
            _ => {}
        },
        "display" => match val.trim().to_ascii_lowercase().as_str() {
            "none" => style.display = Display::None,
            "block" => style.display = Display::Block,
            "inline" => style.display = Display::Inline,
            "inline-block" => style.display = Display::InlineBlock,
            "flex" => style.display = Display::Flex,
            "inline-flex" => style.display = Display::InlineFlex,
            "grid" => style.display = Display::Grid,
            "inline-grid" => style.display = Display::InlineGrid,
            "table" => style.display = Display::Table,
            "table-row" => style.display = Display::TableRow,
            "table-cell" => style.display = Display::TableCell,
            "table-row-group" => style.display = Display::TableRowGroup,
            "table-header-group" => style.display = Display::TableHeaderGroup,
            "table-footer-group" => style.display = Display::TableFooterGroup,
            "table-caption" => style.display = Display::TableCaption,
            "table-column" => style.display = Display::TableColumn,
            "table-column-group" => style.display = Display::TableColumnGroup,
            _ => {}
        },
        "position" => match val.trim().to_ascii_lowercase().as_str() {
            "static" => style.position = Position::Static,
            "relative" => style.position = Position::Relative,
            "absolute" => style.position = Position::Absolute,
            "fixed" => style.position = Position::Fixed,
            "sticky" => style.position = Position::Sticky,
            _ => {}
        },
        // `overflow` (and the `-x`/`-y` longhands): we only need whether the box becomes a scroll
        // container (anything but `visible`), which is the containing block for `sticky` insets.
        "overflow" | "overflow-x" | "overflow-y" => {
            // A single value applies to both axes; the shorthand may carry two. Any non-`visible`
            // (and non-`clip`-without-scrollport — we treat `clip` as a scrollport too, matching the
            // sticky containing-block rule) token marks this box as a scrollport.
            let any_non_visible = val
                .split_whitespace()
                .any(|tok| !matches!(tok.trim().to_ascii_lowercase().as_str(), "visible" | ""));
            if any_non_visible {
                style.overflow_scrollport = true;
            }
        }
        "top" => {
            style.top = parse_length_fs(val, style.font_size);
            style.top_spec = parse_inset_value(val, style.font_size);
        }
        "right" => {
            style.right = parse_length_fs(val, style.font_size);
            style.right_spec = parse_inset_value(val, style.font_size);
        }
        "bottom" => {
            style.bottom = parse_length_fs(val, style.font_size);
            style.bottom_spec = parse_inset_value(val, style.font_size);
        }
        "left" => {
            style.left = parse_length_fs(val, style.font_size);
            style.left_spec = parse_inset_value(val, style.font_size);
        }
        "z-index" => {
            let v = val.trim().to_ascii_lowercase();
            if v == "auto" {
                style.z_index = None;
            } else if let Ok(n) = v.parse::<i32>() {
                style.z_index = Some(n);
            } else if let Ok(n) = v.parse::<i64>() {
                // Out-of-range integers clamp to the representable range (still a valid <integer>).
                style.z_index = Some(n.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
            } else if v.parse::<i128>().is_ok() {
                // Very large integers (beyond i64) still parse as <integer>; clamp by sign.
                style.z_index = Some(if v.starts_with('-') { i32::MIN } else { i32::MAX });
            }
        }

        // --- Flex container ---
        "flex-direction" => match val.trim().to_ascii_lowercase().as_str() {
            "row" => style.flex_direction = FlexDirection::Row,
            "row-reverse" => style.flex_direction = FlexDirection::RowReverse,
            "column" => style.flex_direction = FlexDirection::Column,
            "column-reverse" => style.flex_direction = FlexDirection::ColumnReverse,
            _ => {}
        },
        "flex-wrap" => match val.trim().to_ascii_lowercase().as_str() {
            "nowrap" => style.flex_wrap = FlexWrap::NoWrap,
            "wrap" => style.flex_wrap = FlexWrap::Wrap,
            "wrap-reverse" => style.flex_wrap = FlexWrap::WrapReverse,
            _ => {}
        },
        "flex-flow" => {
            // shorthand: direction and/or wrap, space separated, order-insensitive.
            for tok in val.split_whitespace() {
                let t = tok.to_ascii_lowercase();
                match t.as_str() {
                    "row" => style.flex_direction = FlexDirection::Row,
                    "row-reverse" => style.flex_direction = FlexDirection::RowReverse,
                    "column" => style.flex_direction = FlexDirection::Column,
                    "column-reverse" => style.flex_direction = FlexDirection::ColumnReverse,
                    "nowrap" => style.flex_wrap = FlexWrap::NoWrap,
                    "wrap" => style.flex_wrap = FlexWrap::Wrap,
                    "wrap-reverse" => style.flex_wrap = FlexWrap::WrapReverse,
                    _ => {}
                }
            }
        }
        "justify-content" => {
            if let Some(j) = parse_justify(val) {
                style.justify_content = j;
            }
        }
        "align-items" => match val.trim().to_ascii_lowercase().as_str() {
            "stretch" => style.align_items = AlignItems::Stretch,
            "flex-start" | "start" => style.align_items = AlignItems::FlexStart,
            "flex-end" | "end" => style.align_items = AlignItems::FlexEnd,
            "center" => style.align_items = AlignItems::Center,
            "baseline" => style.align_items = AlignItems::Baseline,
            _ => {}
        },
        "align-content" => {
            if let Some(j) = parse_justify(val) {
                style.align_content = Some(j);
            }
        }

        // --- Flex item ---
        "flex" => apply_flex_shorthand(style, val),
        "flex-grow" => {
            if let Ok(n) = val.trim().parse::<f32>() {
                style.flex_grow = n.max(0.0);
            }
        }
        "flex-shrink" => {
            if let Ok(n) = val.trim().parse::<f32>() {
                style.flex_shrink = n.max(0.0);
            }
        }
        "flex-basis" => {
            let v = val.trim().to_ascii_lowercase();
            style.flex_basis = if v == "auto" { None } else { parse_length(val) };
        }
        "align-self" => match val.trim().to_ascii_lowercase().as_str() {
            "auto" => style.align_self = AlignSelf::Auto,
            "stretch" => style.align_self = AlignSelf::Stretch,
            "flex-start" | "start" => style.align_self = AlignSelf::FlexStart,
            "flex-end" | "end" => style.align_self = AlignSelf::FlexEnd,
            "center" => style.align_self = AlignSelf::Center,
            "baseline" => style.align_self = AlignSelf::Baseline,
            _ => {}
        },
        "order" => {
            if let Ok(n) = val.trim().parse::<i32>() {
                style.order = n;
            }
        }

        // --- Gaps ---
        "gap" => {
            if let Some((r, c)) = parse_gap(val) {
                style.row_gap = r;
                style.column_gap = c;
            }
        }
        "row-gap" => {
            if let Some(v) = parse_length(val) {
                style.row_gap = v;
            }
        }
        "column-gap" => {
            if let Some(v) = parse_length(val) {
                style.column_gap = v;
            }
        }

        // --- Grid ---
        "grid-template-columns" => {
            let tracks = parse_track_list(val);
            if !tracks.is_empty() {
                style.grid_template_columns = tracks;
            }
        }
        "grid-template-rows" => {
            let tracks = parse_track_list(val);
            if !tracks.is_empty() {
                style.grid_template_rows = tracks;
            }
        }
        "grid-column" => {
            style.grid_column = parse_grid_placement(val);
        }
        "grid-row" => {
            style.grid_row = parse_grid_placement(val);
        }

        // --- Box model: margin ---
        "margin" => {
            if let Some(e) = parse_edges_shorthand(val, style.font_size) {
                style.margin = e;
            }
        }
        "margin-top" => set_edge(&mut style.margin, EdgeSide::Top, val, style.font_size),
        "margin-right" => set_edge(&mut style.margin, EdgeSide::Right, val, style.font_size),
        "margin-bottom" => set_edge(&mut style.margin, EdgeSide::Bottom, val, style.font_size),
        "margin-left" => set_edge(&mut style.margin, EdgeSide::Left, val, style.font_size),

        // --- Box model: padding ---
        "padding" => {
            if let Some(e) = parse_edges_shorthand(val, style.font_size) {
                style.padding = e;
            }
        }
        "padding-top" => set_edge(&mut style.padding, EdgeSide::Top, val, style.font_size),
        "padding-right" => set_edge(&mut style.padding, EdgeSide::Right, val, style.font_size),
        "padding-bottom" => set_edge(&mut style.padding, EdgeSide::Bottom, val, style.font_size),
        "padding-left" => set_edge(&mut style.padding, EdgeSide::Left, val, style.font_size),

        // --- Box model: border ---
        "border" => apply_border_shorthand(style, val, EdgeSide::All, current_color, inherited_color),
        "border-top" => apply_border_shorthand(style, val, EdgeSide::Top, current_color, inherited_color),
        "border-right" => apply_border_shorthand(style, val, EdgeSide::Right, current_color, inherited_color),
        "border-bottom" => apply_border_shorthand(style, val, EdgeSide::Bottom, current_color, inherited_color),
        "border-left" => apply_border_shorthand(style, val, EdgeSide::Left, current_color, inherited_color),
        "border-width" => {
            if let Some(e) = parse_edges_shorthand(val, style.font_size) {
                style.border = e;
            }
        }
        "border-top-width" => set_edge(&mut style.border, EdgeSide::Top, val, style.font_size),
        "border-right-width" => set_edge(&mut style.border, EdgeSide::Right, val, style.font_size),
        "border-bottom-width" => set_edge(&mut style.border, EdgeSide::Bottom, val, style.font_size),
        "border-left-width" => set_edge(&mut style.border, EdgeSide::Left, val, style.font_size),
        "border-color" => {
            if let Some(c) = parse_color_ctx(val, current_color, inherited_color) {
                style.border_color = c;
            }
        }

        // --- Table: border-collapse / border-spacing ---
        "border-collapse" => {
            style.border_collapse = match val.trim().to_ascii_lowercase().as_str() {
                "collapse" => BorderCollapse::Collapse,
                _ => BorderCollapse::Separate,
            };
        }
        "border-spacing" => {
            // We model border-spacing as a single scalar (the first length; the row/col
            // distinction is collapsed to one value — a documented simplification).
            if let Some(v) = val.split_whitespace().next().and_then(parse_length) {
                style.border_spacing = v.max(0.0);
            }
        }

        // --- Box model: width / height ---
        "width" => {
            style.width = parse_length(val);
        }
        "height" => {
            style.height = parse_length(val);
        }

        // --- Sizing constraints (min/max) ---
        "min-width" => style.min_width = parse_size_constraint(val),
        "max-width" => style.max_width = parse_size_constraint(val),
        "min-height" => style.min_height = parse_size_constraint(val),
        "max-height" => style.max_height = parse_size_constraint(val),

        // --- Logical / shorthand insets ---
        "inset" => {
            if let Some(e) = parse_optional_edges_shorthand(val) {
                style.top = e.top;
                style.right = e.right;
                style.bottom = e.bottom;
                style.left = e.left;
            }
        }
        "inset-block" => {
            if let Some((a, b)) = parse_pair(val) {
                style.top = a;
                style.bottom = b;
            }
        }
        "inset-inline" => {
            if let Some((a, b)) = parse_pair(val) {
                style.left = a;
                style.right = b;
            }
        }

        // --- Logical padding / margin ---
        "padding-block" => {
            if let Some((a, b)) = parse_edge_pair(val) {
                style.padding.top = a;
                style.padding.bottom = b;
            }
        }
        "padding-inline" => {
            if let Some((a, b)) = parse_edge_pair(val) {
                style.padding.left = a;
                style.padding.right = b;
            }
        }
        "margin-block" => {
            if let Some((a, b)) = parse_edge_pair(val) {
                style.margin.top = a;
                style.margin.bottom = b;
            }
        }
        "margin-inline" => {
            if let Some((a, b)) = parse_edge_pair(val) {
                style.margin.left = a;
                style.margin.right = b;
            }
        }

        // --- Typography extras ---
        "line-height" => {
            if let Some(px) = parse_line_height(val, style.font_size) {
                style.line_height = Some(px);
            }
        }
        "text-transform" => match val.trim().to_ascii_lowercase().as_str() {
            "none" => style.text_transform = TextTransform::None,
            "uppercase" => style.text_transform = TextTransform::Uppercase,
            "lowercase" => style.text_transform = TextTransform::Lowercase,
            "capitalize" => style.text_transform = TextTransform::Capitalize,
            _ => {}
        },
        "letter-spacing" => {
            let v = val.trim().to_ascii_lowercase();
            if v == "normal" {
                style.letter_spacing = 0.0;
            } else if let Some(px) = parse_length(val) {
                style.letter_spacing = px;
            }
        }
        "white-space" => match val.trim().to_ascii_lowercase().as_str() {
            "normal" => style.white_space = WhiteSpace::Normal,
            "nowrap" => style.white_space = WhiteSpace::Nowrap,
            "pre" => style.white_space = WhiteSpace::Pre,
            "pre-wrap" => style.white_space = WhiteSpace::PreWrap,
            // `pre-line` collapses spaces but keeps newlines; approximate as Normal for now.
            _ => {}
        },
        // `list-style-type` (and the `list-style` shorthand, from which we pull the type token).
        "list-style-type" => {
            if let Some(t) = parse_list_style_type(val) {
                style.list_style_type = t;
            }
        }
        "list-style" => {
            // Shorthand: list-style: <type> || <position> || <image>. We only model the type;
            // pull the first token that names a known type (or `none`).
            for tok in val.split_whitespace() {
                if let Some(t) = parse_list_style_type(tok) {
                    style.list_style_type = t;
                    break;
                }
            }
        }

        // --- text-decoration (underline / line-through) ---
        "text-decoration" | "text-decoration-line" => {
            apply_text_decoration(style, val);
        }

        // --- opacity ---
        "opacity" => {
            let v = val.trim().to_ascii_lowercase();
            let n = if let Some(p) = v.strip_suffix('%') {
                p.trim().parse::<f32>().ok().map(|x| x / 100.0)
            } else {
                v.parse::<f32>().ok()
            };
            if let Some(n) = n {
                style.opacity = n.clamp(0.0, 1.0);
            }
        }

        // --- border-radius ---
        "border-radius" => {
            if let Some(r) = parse_border_radius(val) {
                style.border_radius = r;
            }
        }

        // --- generated content (only meaningful on ::before/::after pseudo-elements) ---
        // `attr(name)` references can't be resolved here (we lack the originating element), so they
        // are stored verbatim and resolved by the pseudo cascade via `resolve_content_attr`.
        "content" => {
            style.content = parse_content(val);
        }

        _ => {}
    }
}

/// Parse a `content` value into the string a pseudo-element should render, or `None` when the
/// value generates no box. Handles: a quoted string (with minimal escape handling — `\"`, `\\`,
/// and `\XXXX`/`\XX…` hex unicode escapes); `none`/`normal` → `None`; and `attr(name)`, which is
/// returned verbatim (`attr(name)`) for [`resolve_content_attr`] to resolve once the element is
/// known. Other functional values (`counter(...)`, `url(...)`, multiple tokens, …) are simplified
/// to an empty string (a box with no text).
fn parse_content(val: &str) -> Option<String> {
    let v = val.trim();
    let lower = v.to_ascii_lowercase();
    if lower == "none" || lower == "normal" {
        return None;
    }
    // A single quoted string.
    if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
        || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
    {
        return Some(unescape_content_string(&v[1..v.len() - 1]));
    }
    // `attr(name)` — kept verbatim; resolved against the element later.
    if lower.starts_with("attr(") && v.ends_with(')') {
        return Some(v.to_string());
    }
    // counter(...)/url(...)/anything else we don't model: an empty box (no text), per the
    // documented simplification.
    Some(String::new())
}

/// Decode the minimal CSS string escapes used in `content`: `\"`, `\'`, `\\`, and hex escapes
/// (`\A`, `\2192`, optionally terminated by a space). Unknown escapes drop the backslash.
fn unescape_content_string(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            if next.is_ascii_hexdigit() {
                // Up to 6 hex digits, optionally followed by a single whitespace terminator.
                let mut j = i + 1;
                let mut hex = String::new();
                while j < chars.len() && hex.len() < 6 && chars[j].is_ascii_hexdigit() {
                    hex.push(chars[j]);
                    j += 1;
                }
                if j < chars.len() && chars[j] == ' ' {
                    j += 1; // consume the terminating space
                }
                if let Some(cp) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(cp);
                }
                i = j;
                continue;
            }
            // Literal escape (`\"`, `\\`, …): emit the escaped char.
            out.push(next);
            i += 2;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Resolve a parsed `content` string against the originating element: if it's an `attr(name)`
/// reference, return the element's `name` attribute value (or empty string when absent);
/// otherwise return it unchanged.
fn resolve_content_attr(content: &str, el: &dom::ElementData) -> String {
    let lower = content.to_ascii_lowercase();
    if lower.starts_with("attr(") && content.ends_with(')') {
        let name = content[5..content.len() - 1].trim();
        return el.attrs.get(&name.to_ascii_lowercase())
            .or_else(|| el.attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v))
            .cloned()
            .unwrap_or_default();
    }
    content.to_string()
}

/// Parse a `min-width`/`max-width`/`min-height`/`max-height` value. `none`/`auto`/empty → `None`
/// (no constraint). Supports px (and pt/unitless via [`parse_length`]) and `%`.
fn parse_size_constraint(val: &str) -> Option<SizeConstraint> {
    let v = val.trim().to_ascii_lowercase();
    if v.is_empty() || v == "none" || v == "auto" {
        return None;
    }
    if has_math_func(&v) {
        return eval_length(&v, 16.0).map(SizeConstraint::Px);
    }
    if let Some(p) = v.strip_suffix('%') {
        return p.trim().parse::<f32>().ok().map(|x| SizeConstraint::Pct(x / 100.0));
    }
    parse_length(val).map(SizeConstraint::Px)
}

/// Parse an `inset` shorthand of 1–4 values into per-side `Option<f32>` (auto → None).
/// CSS order: `all` / `vert horiz` / `top horiz bottom` / `top right bottom left`.
struct OptionalEdges {
    top: Option<f32>,
    right: Option<f32>,
    bottom: Option<f32>,
    left: Option<f32>,
}

fn parse_optional_edges_shorthand(val: &str) -> Option<OptionalEdges> {
    let parts: Vec<Option<f32>> = val.split_whitespace().map(parse_length).collect();
    match parts.len() {
        1 => Some(OptionalEdges { top: parts[0], right: parts[0], bottom: parts[0], left: parts[0] }),
        2 => Some(OptionalEdges { top: parts[0], bottom: parts[0], right: parts[1], left: parts[1] }),
        3 => Some(OptionalEdges { top: parts[0], right: parts[1], left: parts[1], bottom: parts[2] }),
        n if n >= 4 => Some(OptionalEdges { top: parts[0], right: parts[1], bottom: parts[2], left: parts[3] }),
        _ => None,
    }
}

/// Parse a 1–2 value list into `(first, second)` of `Option<f32>` (used by inset-block/inline);
/// a single value applies to both sides.
fn parse_pair(val: &str) -> Option<(Option<f32>, Option<f32>)> {
    let parts: Vec<&str> = val.split_whitespace().collect();
    match parts.len() {
        1 => {
            let a = parse_length(parts[0]);
            Some((a, a))
        }
        n if n >= 2 => Some((parse_length(parts[0]), parse_length(parts[1]))),
        _ => None,
    }
}

/// Like [`parse_pair`] but for padding/margin edges (`auto`/`none` → 0), returning concrete f32.
fn parse_edge_pair(val: &str) -> Option<(f32, f32)> {
    let parts: Vec<f32> = val
        .split_whitespace()
        .map(|t| parse_edge_length(t, 16.0).unwrap_or(0.0))
        .collect();
    match parts.len() {
        1 => Some((parts[0], parts[0])),
        n if n >= 2 => Some((parts[0], parts[1])),
        _ => None,
    }
}

/// Parse `line-height`: unitless number (× font-size), `px`, or `%`/`em`/`rem` (× font-size,
/// rem × 16). `normal` → `None` (use the font metric). Returns resolved px.
fn parse_line_height(val: &str, font_size: f32) -> Option<f32> {
    let v = val.trim().to_ascii_lowercase();
    if v.is_empty() || v == "normal" {
        return None;
    }
    if has_math_func(&v) {
        return eval_length(&v, font_size);
    }
    if let Some(p) = v.strip_suffix('%') {
        return p.trim().parse::<f32>().ok().map(|x| x / 100.0 * font_size);
    }
    if let Some(e) = v.strip_suffix("rem") {
        return e.trim().parse::<f32>().ok().map(|x| x * 16.0);
    }
    if let Some(e) = v.strip_suffix("em") {
        return e.trim().parse::<f32>().ok().map(|x| x * font_size);
    }
    if let Some(px) = v.strip_suffix("px") {
        return px.trim().parse::<f32>().ok();
    }
    if let Some(pt) = v.strip_suffix("pt") {
        return pt.trim().parse::<f32>().ok().map(|x| x * 4.0 / 3.0);
    }
    // Unitless: a multiple of the font size.
    v.parse::<f32>().ok().map(|x| x * font_size)
}

/// Apply a `text-decoration`/`text-decoration-line` value: detect `underline` / `line-through` /
/// `overline` / `none` keywords (color/style tokens ignored). `none` clears both flags.
fn apply_text_decoration(style: &mut ComputedStyle, val: &str) {
    let lower = val.to_ascii_lowercase();
    if lower.split_whitespace().any(|t| t == "none") {
        style.underline = false;
        style.line_through = false;
        style.overline = false;
        return;
    }
    for tok in lower.split_whitespace() {
        match tok {
            "underline" => style.underline = true,
            "line-through" => style.line_through = true,
            "overline" => style.overline = true,
            _ => {}
        }
    }
}

/// Parse `border-radius` (1–4 values). We take the *first* radius and use it uniformly (per-corner
/// and elliptical `/` syntax are simplified away). `%` resolves to `None` here (can't resolve
/// without box size) → falls back to 0; px/unitless resolve directly.
fn parse_border_radius(val: &str) -> Option<f32> {
    // A single math function (which may itself contain spaces / a `/`) is evaluated whole.
    if has_math_func(val) {
        return eval_length(val, 16.0).map(|r| r.max(0.0));
    }
    // Ignore the elliptical `a / b` part: use the horizontal radii before `/`.
    let main = val.split('/').next().unwrap_or(val);
    let first = main.split_whitespace().next()?;
    let lower = first.trim().to_ascii_lowercase();
    if lower.ends_with('%') {
        // Percentage radius unsupported (needs box size); approximate as 0 → square.
        return Some(0.0);
    }
    parse_length(first).map(|r| r.max(0.0))
}

/// Split `s` on top-level commas (not inside parens). Returns trimmed non-empty parts.
fn split_top_commas(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    for (i, &c) in chars.iter().enumerate() {
        match c {
            '(' => depth += 1,
            ')' => depth = (depth - 1).max(0),
            ',' if depth == 0 => {
                let p: String = chars[start..i].iter().collect();
                let p = p.trim();
                if !p.is_empty() {
                    parts.push(p.to_string());
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let p: String = chars[start..].iter().collect();
    let p = p.trim();
    if !p.is_empty() {
        parts.push(p.to_string());
    }
    parts
}

/// Parse an angle token (`90deg`, `0.5turn`, `1.57rad`, bare number=deg) to degrees.
fn parse_angle_deg(tok: &str) -> Option<f32> {
    let t = tok.trim().to_ascii_lowercase();
    if let Some(n) = t.strip_suffix("deg") {
        n.trim().parse::<f32>().ok()
    } else if let Some(n) = t.strip_suffix("grad") {
        n.trim().parse::<f32>().ok().map(|x| x * 0.9)
    } else if let Some(n) = t.strip_suffix("rad") {
        n.trim().parse::<f32>().ok().map(|x| x.to_degrees())
    } else if let Some(n) = t.strip_suffix("turn") {
        n.trim().parse::<f32>().ok().map(|x| x * 360.0)
    } else {
        t.parse::<f32>().ok()
    }
}

/// Resolve a relative CSS `url(...)` value against the stylesheet's `base` URL, returning an
/// absolute URL. `data:` URLs and anything that fails to resolve (e.g. no base, or `base` isn't a
/// valid absolute URL) are returned unchanged — the engine then falls back to resolving against the
/// document URL. This is what makes `url('../icons/x.svg')` in an external sheet load from the
/// sheet's directory, not the document's.
fn resolve_css_url(url: &str, base: Option<&str>) -> String {
    let trimmed = url.trim();
    // `data:` URLs are already self-contained; never rewrite them.
    if trimmed.to_ascii_lowercase().starts_with("data:") {
        return trimmed.to_string();
    }
    let Some(base) = base else {
        return trimmed.to_string();
    };
    match url::Url::parse(base).and_then(|b| b.join(trimmed)) {
        Ok(joined) => joined.into(),
        Err(_) => trimmed.to_string(),
    }
}

/// Parse a `mask` / `mask-image` value into a [`MaskImage`]. Extracts the first `url(...)` source
/// (with surrounding quotes stripped) and scans for a `contain` / `cover` size keyword (the part
/// after `/` in the shorthand). Other tokens (`no-repeat`, `center`, position, etc.) are ignored.
/// Returns `None` when there's no `url(...)` (e.g. a gradient-as-mask, which is out of scope).
fn parse_mask(val: &str) -> Option<MaskImage> {
    let lower = val.to_ascii_lowercase();
    // Find the first `url(` and its matching `)`.
    let start = lower.find("url(")?;
    let inner_start = start + 4;
    let rest = &val[inner_start..];
    let close = rest.find(')')?;
    let mut raw = rest[..close].trim().to_string();
    // Strip optional surrounding quotes.
    if (raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2)
        || (raw.starts_with('\'') && raw.ends_with('\'') && raw.len() >= 2)
    {
        raw = raw[1..raw.len() - 1].to_string();
    }
    let url = raw.trim().to_string();
    if url.is_empty() {
        return None;
    }
    // Size keyword: look at the tokens AFTER the `/` (CSS `position / size`), else scan the whole
    // value for `contain`/`cover`. Default is `Stretch` (no keyword → fit-to-box).
    let after_url = &lower[inner_start + close + 1..];
    let size = if after_url.contains("cover") {
        MaskSize::Cover
    } else if after_url.contains("contain") {
        MaskSize::Contain
    } else {
        MaskSize::Stretch
    };
    Some(MaskImage { url, size })
}

/// Parse a `linear-gradient(...)` / `radial-gradient(...)` (incl. `repeating-*`) value into a
/// [`Gradient`], or `None` if the value isn't a recognized gradient. Color stops without an
/// explicit position are distributed evenly between their neighbors (0..1). Stop positions
/// expressed as `%` resolve directly; `px` lengths are resolved as a fraction of an assumed
/// 200px gradient line (best-effort, since the real line length isn't known until paint).
fn parse_gradient(val: &str, current: (u8, u8, u8), inherited: (u8, u8, u8)) -> Option<Gradient> {
    let v = val.trim();
    let lower = v.to_ascii_lowercase();
    let (is_radial, body) = if let Some(rest) = strip_func(&lower, v, "linear-gradient") {
        (false, rest)
    } else if let Some(rest) = strip_func(&lower, v, "repeating-linear-gradient") {
        (false, rest)
    } else if let Some(rest) = strip_func(&lower, v, "radial-gradient") {
        (true, rest)
    } else if let Some(rest) = strip_func(&lower, v, "repeating-radial-gradient") {
        (true, rest)
    } else {
        return None;
    };

    let mut parts = split_top_commas(body);
    if parts.is_empty() {
        return None;
    }

    // The first part may be a direction/angle (linear) or a shape/size/position prelude (radial)
    // rather than a color stop. Detect by checking whether it parses as a color stop.
    let mut angle_deg = 180.0_f32; // default: to bottom
    let first_lower = parts[0].to_ascii_lowercase();
    let first_is_prelude = if is_radial {
        // Radial prelude starts with a shape/size keyword or `at`.
        first_lower.starts_with("at ")
            || first_lower.contains(" at ")
            || first_lower.starts_with("circle")
            || first_lower.starts_with("ellipse")
            || first_lower.contains("closest")
            || first_lower.contains("farthest")
    } else {
        first_lower.starts_with("to ") || parse_angle_deg(&first_lower).is_some()
    };
    if first_is_prelude {
        if !is_radial {
            angle_deg = parse_linear_direction(&first_lower).unwrap_or(180.0);
        }
        parts.remove(0);
    }

    let mut stops: Vec<GradientStop> = Vec::new();
    // Parse each "color [pos]" stop; remember which positions were explicit for distribution.
    let mut explicit: Vec<Option<f32>> = Vec::new();
    for part in &parts {
        // Split the color from a trailing position. The color may contain spaces (rgb( ... )),
        // so split off a trailing token only if it looks like a position (ends with % or a unit).
        let (color_str, pos) = split_stop(part);
        let color = parse_rgba_ctx(color_str, current, inherited)?;
        stops.push(GradientStop { color, pos: 0.0 });
        explicit.push(pos);
    }
    if stops.len() < 2 {
        return None;
    }

    // Distribute positions: clamp to 0..1, default ends to 0 and 1, interpolate gaps.
    let n = stops.len();
    if explicit[0].is_none() {
        explicit[0] = Some(0.0);
    }
    if explicit[n - 1].is_none() {
        explicit[n - 1] = Some(1.0);
    }
    let mut i = 0;
    while i < n {
        if explicit[i].is_some() {
            i += 1;
            continue;
        }
        // Find the next explicit stop.
        let prev = explicit[i - 1].unwrap();
        let mut j = i;
        while j < n && explicit[j].is_none() {
            j += 1;
        }
        let next = explicit[j].unwrap();
        let gap = (j - (i - 1)) as f32;
        for k in i..j {
            let frac = (k - (i - 1)) as f32 / gap;
            explicit[k] = Some(prev + (next - prev) * frac);
        }
        i = j;
    }
    for (s, p) in stops.iter_mut().zip(explicit.iter()) {
        s.pos = p.unwrap().clamp(0.0, 1.0);
    }
    // Ensure non-decreasing positions.
    for k in 1..n {
        if stops[k].pos < stops[k - 1].pos {
            stops[k].pos = stops[k - 1].pos;
        }
    }

    if is_radial {
        Some(Gradient::Radial { stops })
    } else {
        Some(Gradient::Linear { angle_deg, stops })
    }
}

/// If `lower` (the lowercased value) starts with `name(` and `v` ends with `)`, return the inner
/// body (from the original-case `v`). Else `None`.
fn strip_func<'a>(lower: &str, v: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name}(");
    if lower.starts_with(&prefix) && v.ends_with(')') {
        Some(&v[prefix.len()..v.len() - 1])
    } else {
        None
    }
}

/// Parse a linear-gradient direction (`to right`, `to top left`, or an angle) into degrees in the
/// CSS convention (0=to top, 90=to right, 180=to bottom, 270=to left).
fn parse_linear_direction(s: &str) -> Option<f32> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("to ") {
        let mut to_top = false;
        let mut to_bottom = false;
        let mut to_left = false;
        let mut to_right = false;
        for kw in rest.split_whitespace() {
            match kw {
                "top" => to_top = true,
                "bottom" => to_bottom = true,
                "left" => to_left = true,
                "right" => to_right = true,
                _ => {}
            }
        }
        let deg = match (to_top, to_bottom, to_left, to_right) {
            (true, _, false, false) => 0.0,
            (false, true, false, false) => 180.0,
            (false, false, true, false) => 270.0,
            (false, false, false, true) => 90.0,
            (true, _, false, true) => 45.0,
            (true, _, true, false) => 315.0,
            (false, true, false, true) => 135.0,
            (false, true, true, false) => 225.0,
            _ => 180.0,
        };
        return Some(deg);
    }
    parse_angle_deg(s)
}

/// Split a gradient color-stop into `(color_str, Option<position 0..1>)`. The position is the
/// trailing token if it ends with `%` or a length unit; `%` resolves directly, `px` against an
/// assumed 200px line.
fn split_stop(part: &str) -> (&str, Option<f32>) {
    let trimmed = part.trim();
    // Find the last whitespace-delimited token.
    if let Some(idx) = trimmed.rfind(char::is_whitespace) {
        let last = trimmed[idx + 1..].trim();
        let pos = if let Some(p) = last.strip_suffix('%') {
            p.trim().parse::<f32>().ok().map(|x| x / 100.0)
        } else if last.ends_with("px") || last.ends_with("rem") || last.ends_with("em") {
            parse_length(last).map(|px| px / 200.0)
        } else {
            None
        };
        if pos.is_some() {
            return (trimmed[..idx].trim(), pos);
        }
    }
    (trimmed, None)
}

/// Parse a `box-shadow` value (comma-separated list) into [`BoxShadow`] layers. Each layer is
/// `[inset]? <dx> <dy> [<blur>] [<spread>] [<color>]`. Color defaults to `current` (currentColor).
/// Returns an empty vec if nothing parsed.
fn parse_box_shadows(val: &str, current: (u8, u8, u8), inherited: (u8, u8, u8)) -> Vec<BoxShadow> {
    let v = val.trim();
    if v.eq_ignore_ascii_case("none") || v.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for layer in split_top_commas(v) {
        let mut inset = false;
        let mut lengths: Vec<f32> = Vec::new();
        let mut color: Option<Rgba> = None;
        // Tokenize respecting parens (so `rgba(0,0,0,.5)` stays one token).
        for tok in tokenize_paren_aware(&layer) {
            let tl = tok.to_ascii_lowercase();
            if tl == "inset" {
                inset = true;
                continue;
            }
            if lengths.len() < 4 {
                if let Some(px) = parse_length(&tok) {
                    lengths.push(px);
                    continue;
                }
            }
            if color.is_none() {
                if let Some(c) = parse_rgba_ctx(&tok, current, inherited) {
                    color = Some(c);
                    continue;
                }
            }
        }
        if lengths.len() < 2 {
            continue; // need at least dx, dy
        }
        out.push(BoxShadow {
            inset,
            dx: lengths[0],
            dy: lengths[1],
            blur: lengths.get(2).copied().unwrap_or(0.0).max(0.0),
            spread: lengths.get(3).copied().unwrap_or(0.0),
            color: color.unwrap_or(Rgba { r: current.0, g: current.1, b: current.2, a: 255 }),
        });
    }
    out
}

/// Tokenize a value on whitespace, keeping balanced parens together (so functional colors with
/// internal spaces/commas survive as one token).
fn tokenize_paren_aware(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut i = 0;
    let mut in_tok = false;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() && depth == 0 {
            if in_tok {
                out.push(chars[start..i].iter().collect());
                in_tok = false;
            }
        } else {
            if !in_tok {
                start = i;
                in_tok = true;
            }
            if c == '(' {
                depth += 1;
            } else if c == ')' {
                depth = (depth - 1).max(0);
            }
        }
        i += 1;
    }
    if in_tok {
        out.push(chars[start..].iter().collect());
    }
    out
}

/// Parse a `transform` value (a space-separated list of functions) into a composed 2D affine
/// `[a b c d e f]` (column-major-ish: x'=a*x+c*y+e, y'=b*x+d*y+f). Supported: `translate`,
/// `translateX`/`Y`, `scale`/`X`/`Y`, `rotate`, `matrix`. `skewX`/`skewY` are best-effort
/// (applied as shear); unknown functions are skipped. Percentages in `translate` are left as 0
/// here and resolved at paint time against the box size — see [`transform_translate_pct`].
/// Returns `None` if no function parsed (so the caller leaves transform unset).
fn parse_transform(val: &str) -> Option<[f32; 6]> {
    let v = val.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("none") {
        return None;
    }
    let mut m = IDENTITY;
    let mut any = false;
    for (name, args) in transform_functions(v) {
        let nums: Vec<f32> = split_top_commas(&args)
            .iter()
            .filter_map(|a| transform_arg(a))
            .collect();
        let t = match name.as_str() {
            "translate" => {
                let x = nums.first().copied().unwrap_or(0.0);
                let y = nums.get(1).copied().unwrap_or(0.0);
                [1.0, 0.0, 0.0, 1.0, x, y]
            }
            "translatex" => [1.0, 0.0, 0.0, 1.0, nums.first().copied().unwrap_or(0.0), 0.0],
            "translatey" => [1.0, 0.0, 0.0, 1.0, 0.0, nums.first().copied().unwrap_or(0.0)],
            "scale" => {
                let sx = nums.first().copied().unwrap_or(1.0);
                let sy = nums.get(1).copied().unwrap_or(sx);
                [sx, 0.0, 0.0, sy, 0.0, 0.0]
            }
            "scalex" => [nums.first().copied().unwrap_or(1.0), 0.0, 0.0, 1.0, 0.0, 0.0],
            "scaley" => [1.0, 0.0, 0.0, nums.first().copied().unwrap_or(1.0), 0.0, 0.0],
            "rotate" => {
                let deg = parse_angle_deg(&args).unwrap_or(0.0);
                let r = deg.to_radians();
                [r.cos(), r.sin(), -r.sin(), r.cos(), 0.0, 0.0]
            }
            "skewx" => {
                let deg = parse_angle_deg(&args).unwrap_or(0.0);
                [1.0, 0.0, deg.to_radians().tan(), 1.0, 0.0, 0.0]
            }
            "skewy" => {
                let deg = parse_angle_deg(&args).unwrap_or(0.0);
                [1.0, deg.to_radians().tan(), 0.0, 1.0, 0.0, 0.0]
            }
            "matrix" => {
                if nums.len() == 6 {
                    [nums[0], nums[1], nums[2], nums[3], nums[4], nums[5]]
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        m = mat_mul(m, t);
        any = true;
    }
    if any { Some(m) } else { None }
}

/// The 2D-affine identity.
const IDENTITY: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// Multiply two affines `a` then `b` applied as `a * b` (apply `b` first, then `a`), matching CSS
/// left-to-right function composition (first listed transform is outermost).
fn mat_mul(a: [f32; 6], b: [f32; 6]) -> [f32; 6] {
    // a = [a0 a2 a4; a1 a3 a5], b similarly. result = a · b (3x3 augmented).
    [
        a[0] * b[0] + a[2] * b[1],
        a[1] * b[0] + a[3] * b[1],
        a[0] * b[2] + a[2] * b[3],
        a[1] * b[2] + a[3] * b[3],
        a[0] * b[4] + a[2] * b[5] + a[4],
        a[1] * b[4] + a[3] * b[5] + a[5],
    ]
}

/// Parse a `transform` argument to px/number (`deg`/etc. handled by callers; `%` → left as 0,
/// since the real basis is the box size, resolved at paint).
fn transform_arg(a: &str) -> Option<f32> {
    let t = a.trim();
    if t.ends_with('%') {
        return Some(0.0); // percentage translate resolved at paint time (approx: ignore here)
    }
    parse_length(t).or_else(|| t.parse::<f32>().ok())
}

/// Split a transform value into `(function_name_lowercased, args_string)` pairs.
fn transform_functions(s: &str) -> Vec<(String, String)> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == ',') {
            i += 1;
        }
        let name_start = i;
        while i < chars.len() && chars[i] != '(' {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let name: String = chars[name_start..i].iter().collect::<String>().trim().to_ascii_lowercase();
        i += 1; // skip '('
        let args_start = i;
        let mut depth = 1i32;
        while i < chars.len() && depth > 0 {
            match chars[i] {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                break;
            }
            i += 1;
        }
        let args: String = chars[args_start..i].iter().collect();
        i += 1; // skip ')'
        if !name.is_empty() {
            out.push((name, args));
        }
    }
    out
}

/// Parse `transform-origin` into (x, y) fractions of the box size. Supports keywords
/// (`left`/`right`/`top`/`bottom`/`center`) and percentages; px values are approximated as a
/// fraction of an assumed 200px box (best-effort). Default (0.5, 0.5).
fn parse_transform_origin(val: &str) -> (f32, f32) {
    let toks: Vec<String> = val.split_whitespace().map(|t| t.to_ascii_lowercase()).collect();
    let mut x = 0.5;
    let mut y = 0.5;
    let resolve = |t: &str, _horizontal: bool| -> Option<f32> {
        match t {
            "left" | "top" => Some(0.0),
            "right" | "bottom" => Some(1.0),
            "center" => Some(0.5),
            _ => {
                if let Some(p) = t.strip_suffix('%') {
                    p.trim().parse::<f32>().ok().map(|v| v / 100.0)
                } else {
                    parse_length(t).map(|px| px / 200.0)
                }
            }
        }
    };
    // Keywords can appear in either order; handle the common 1-2 token forms positionally,
    // promoting vertical keywords to y.
    match toks.len() {
        1 => {
            let t = &toks[0];
            if t == "top" || t == "bottom" {
                if let Some(v) = resolve(t, false) {
                    y = v;
                }
            } else if let Some(v) = resolve(t, true) {
                x = v;
            }
        }
        n if n >= 2 => {
            // Detect swapped order (e.g. "top left").
            let (a, b) = (&toks[0], &toks[1]);
            let a_vert = a == "top" || a == "bottom";
            let b_horiz = b == "left" || b == "right";
            if a_vert && b_horiz {
                if let Some(v) = resolve(a, false) {
                    y = v;
                }
                if let Some(v) = resolve(b, true) {
                    x = v;
                }
            } else {
                if let Some(v) = resolve(a, true) {
                    x = v;
                }
                if let Some(v) = resolve(b, false) {
                    y = v;
                }
            }
        }
        _ => {}
    }
    (x, y)
}

/// Which side(s) of a box a value targets.
#[derive(Clone, Copy)]
pub enum EdgeSide {
    Top,
    Right,
    Bottom,
    Left,
    All,
}

/// Evaluate a CSS length value that may use the math functions `min()`, `max()`, `clamp()`, and
/// `calc()`, resolving to a final px `f32`. `font_size_px` is the element's font size, used to
/// resolve `em` (and is the basis for `%` would-be percentages — but percentages in lengths are
/// resolved here against [`assumed_viewport_width()`] as an approximation, since the real
/// percentage basis isn't known until layout). Units handled: `px`, `rem` (×16), `em`
/// (×`font_size_px`), `pt` (×4/3), `vw` (=`assumed_viewport_width()`/100×n), `vh`
/// (=`assumed_viewport_height()`/100×n), `%` (×`assumed_viewport_width()`/100 — approximate), and a
/// bare unitless number (used as-is, e.g. multipliers / `calc(2 * 3px)`). Nested functions are
/// supported. Any unknown unit/function or a parse failure yields `None` so callers fall back to
/// their existing behavior; it never panics.
///
/// Returns `None` for plain lengths that contain no math function — callers should only reach for
/// this when a `(`/math token is present, then fall back to their own parser.
fn eval_length(value: &str, font_size_px: f32) -> Option<f32> {
    let lower = value.trim().to_ascii_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut p = MathParser { chars: &chars, pos: 0, font_size: font_size_px };
    p.skip_ws();
    let v = p.parse_expr()?;
    p.skip_ws();
    if p.pos != p.chars.len() {
        return None; // trailing garbage → bail
    }
    if v.is_finite() {
        Some(v)
    } else {
        None
    }
}

/// A tiny recursive-descent evaluator for CSS length math (`calc`/`min`/`max`/`clamp` and the
/// terms they contain). Operates on a lowercased char slice. Each evaluated value is already
/// resolved to px (or a unitless number for bare numbers / multipliers).
struct MathParser<'a> {
    chars: &'a [char],
    pos: usize,
    font_size: f32,
}

impl<'a> MathParser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.chars.len() && self.chars[self.pos].is_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    /// `expr := term (('+' | '-') term)*`
    fn parse_expr(&mut self) -> Option<f32> {
        let mut acc = self.parse_term()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('+') => {
                    self.pos += 1;
                    acc += self.parse_term()?;
                }
                Some('-') => {
                    self.pos += 1;
                    acc -= self.parse_term()?;
                }
                _ => break,
            }
        }
        Some(acc)
    }

    /// `term := factor (('*' | '/') factor)*`
    fn parse_term(&mut self) -> Option<f32> {
        let mut acc = self.parse_factor()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('*') => {
                    self.pos += 1;
                    acc *= self.parse_factor()?;
                }
                Some('/') => {
                    self.pos += 1;
                    let d = self.parse_factor()?;
                    if d == 0.0 {
                        return None;
                    }
                    acc /= d;
                }
                _ => break,
            }
        }
        Some(acc)
    }

    /// `factor := '(' expr ')' | func | number-with-unit`
    fn parse_factor(&mut self) -> Option<f32> {
        self.skip_ws();
        match self.peek()? {
            '(' => {
                self.pos += 1;
                let v = self.parse_expr()?;
                self.skip_ws();
                if self.peek() == Some(')') {
                    self.pos += 1;
                    Some(v)
                } else {
                    None
                }
            }
            '+' => {
                // Unary plus.
                self.pos += 1;
                self.parse_factor()
            }
            '-' => {
                // Unary minus.
                self.pos += 1;
                self.parse_factor().map(|v| -v)
            }
            c if c.is_ascii_alphabetic() => self.parse_function(),
            _ => self.parse_value(),
        }
    }

    /// Parse a `min()/max()/clamp()/calc()` call (the identifier and its parenthesized,
    /// comma-separated argument list).
    fn parse_function(&mut self) -> Option<f32> {
        let name_start = self.pos;
        while self.pos < self.chars.len()
            && (self.chars[self.pos].is_ascii_alphabetic() || self.chars[self.pos] == '-')
        {
            self.pos += 1;
        }
        let name: String = self.chars[name_start..self.pos].iter().collect();
        self.skip_ws();
        if self.peek() != Some('(') {
            return None;
        }
        self.pos += 1; // consume '('
        let mut args: Vec<f32> = Vec::new();
        loop {
            let v = self.parse_expr()?;
            args.push(v);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                    continue;
                }
                Some(')') => {
                    self.pos += 1;
                    break;
                }
                _ => return None,
            }
        }
        match name.as_str() {
            "calc" => {
                if args.len() == 1 {
                    Some(args[0])
                } else {
                    None
                }
            }
            "min" => args.iter().cloned().reduce(f32::min),
            "max" => args.iter().cloned().reduce(f32::max),
            "clamp" => {
                if args.len() == 3 {
                    // clamp(lo, val, hi) == max(lo, min(val, hi))
                    Some(args[0].max(args[1].min(args[2])))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Parse a single numeric token with an optional unit, resolving it to px (or a unitless
    /// number). The numeric part may be a float; the unit is a trailing run of letters or `%`.
    fn parse_value(&mut self) -> Option<f32> {
        let start = self.pos;
        while self.pos < self.chars.len()
            && (self.chars[self.pos].is_ascii_digit() || self.chars[self.pos] == '.')
        {
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        let num: f32 = self.chars[start..self.pos]
            .iter()
            .collect::<String>()
            .parse()
            .ok()?;
        // Read a trailing unit (letters or `%`).
        let unit_start = self.pos;
        while self.pos < self.chars.len()
            && (self.chars[self.pos].is_ascii_alphabetic() || self.chars[self.pos] == '%')
        {
            self.pos += 1;
        }
        let unit: String = self.chars[unit_start..self.pos].iter().collect();
        match unit.as_str() {
            "" => Some(num), // unitless number (multiplier / line-height factor)
            "px" => Some(num),
            "rem" => Some(num * 16.0),
            "em" => Some(num * self.font_size),
            "pt" => Some(num * 4.0 / 3.0),
            "vw" => Some(num * assumed_viewport_width() / 100.0),
            "vh" => Some(num * assumed_viewport_height() / 100.0),
            "vmin" => Some(num * assumed_viewport_width().min(assumed_viewport_height()) / 100.0),
            "vmax" => Some(num * assumed_viewport_width().max(assumed_viewport_height()) / 100.0),
            // Percentages in a length: no real basis at cascade time; approximate against the
            // assumed viewport width.
            "%" => Some(num / 100.0 * assumed_viewport_width()),
            _ => None, // unknown unit
        }
    }
}

/// True if a value contains a length math function we can evaluate (`calc`/`min`/`max`/`clamp`).
fn has_math_func(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("calc(")
        || lower.contains("min(")
        || lower.contains("max(")
        || lower.contains("clamp(")
}

/// Parse a single `list-style-type` keyword into a [`ListStyleType`] (None for unknown tokens).
fn parse_list_style_type(val: &str) -> Option<ListStyleType> {
    match val.trim().to_ascii_lowercase().as_str() {
        "disc" => Some(ListStyleType::Disc),
        "circle" => Some(ListStyleType::Circle),
        "square" => Some(ListStyleType::Square),
        "decimal" => Some(ListStyleType::Decimal),
        "none" => Some(ListStyleType::None),
        _ => None,
    }
}

/// Parse a CSS length to px. Accepts `Npx`, `Npt` (×4/3), and bare numbers (px). `auto`,
/// percentages, and unparseable values yield `None`. `0` (unitless) yields `Some(0)`.
/// Length math functions (`calc`/`min`/`max`/`clamp`) are evaluated via [`eval_length`] (with a
/// default 16px font size for `em`, since this parser has no element context).
fn parse_length(val: &str) -> Option<f32> {
    parse_length_fs(val, 16.0)
}

/// Like [`parse_length`] but resolves `em` against the supplied element `font_size` (CSS px). The
/// non-em paths are identical. Used for box-model edges (margin/padding), where the UA sheet uses
/// `em` values that must scale with each element's font size (e.g. `h1 { margin: 0.67em 0 }`).
fn parse_length_fs(val: &str, font_size: f32) -> Option<f32> {
    let v = val.trim().to_ascii_lowercase();
    if v.is_empty() || v == "auto" {
        return None;
    }
    if has_math_func(&v) {
        return eval_length(&v, font_size);
    }
    if v.ends_with('%') {
        return None; // percentages unsupported for now
    }
    let num = |suffix: &str| v.strip_suffix(suffix).and_then(|n| n.trim().parse::<f32>().ok());
    if let Some(px) = num("px") {
        Some(px)
    } else if let Some(pt) = num("pt") {
        Some(pt * 4.0 / 3.0)
    } else if let Some(em) = num("rem") {
        Some(em * 16.0)
    } else if let Some(em) = num("em") {
        // em resolves against the element's own font size.
        Some(em * font_size)
    } else {
        v.parse::<f32>().ok()
    }
}

/// Split a declaration value into `(value_without_importance, is_important)`. A trailing
/// `!important` (case-insensitive, with optional whitespace around the `!`) sets the flag and is
/// stripped so the remaining value parses cleanly.
fn split_importance(val: &str) -> (&str, bool) {
    let trimmed = val.trim_end();
    // Find a trailing "important" keyword preceded (somewhere) by "!".
    let lower = trimmed.to_ascii_lowercase();
    if let Some(pos) = lower.rfind("important") {
        if pos + "important".len() == lower.len() {
            // Everything before "important" must end with optional ws then `!`.
            let before = trimmed[..pos].trim_end();
            if let Some(stripped) = before.strip_suffix('!') {
                return (stripped.trim_end(), true);
            }
        }
    }
    (trimmed, false)
}

/// Parse the *specified* value of an inset longhand into an [`InsetValue`], retaining percentages
/// and percentage-bearing `calc()` symbolically (their basis isn't known until layout). Absolute
/// lengths (incl. `em`/`rem`) are absolutized to px via [`parse_length_fs`]. The `calc()` parsing
/// handles the simple `<percentage> ± <length>` and bare `<percentage>` forms the inset WPT tests
/// use (`calc(10% - 1px)`); richer calc still resolves its length part and any percentage.
fn parse_inset_value(val: &str, font_size: f32) -> InsetValue {
    let v = val.trim().to_ascii_lowercase();
    if v.is_empty() || v == "auto" {
        return InsetValue::Auto;
    }
    // Plain percentage: `10%`.
    if let Some(p) = v.strip_suffix('%').and_then(|n| n.trim().parse::<f32>().ok()) {
        return InsetValue::Percent(p);
    }
    // calc() / math functions that mention a percentage: split into percentage + length terms.
    if has_math_func(&v) {
        if v.contains('%') {
            if let Some(iv) = parse_calc_percent(&v, font_size) {
                return iv;
            }
        }
        // No percentage (or unparseable): fall back to a fully-absolutized length.
        if let Some(px) = eval_length(&v, font_size) {
            return InsetValue::Length(px);
        }
        return InsetValue::Auto;
    }
    match parse_length_fs(&v, font_size) {
        Some(px) => InsetValue::Length(px),
        None => InsetValue::Auto,
    }
}

/// Parse a `calc()` of the form `calc(<percentage> [+|-] <length>)` (or just `calc(<percentage>)`)
/// into an [`InsetValue::Calc`]. The length part is absolutized to px. Terms may appear in either
/// order. Returns `None` if the expression isn't this shape (caller falls back).
fn parse_calc_percent(val: &str, font_size: f32) -> Option<InsetValue> {
    // Strip the outer `calc(...)`.
    let inner = val.trim().strip_prefix("calc(")?.strip_suffix(')')?.trim();
    let mut pct = 0.0f32;
    let mut px = 0.0f32;
    let mut found_pct = false;
    // Split into signed terms. We scan for top-level `+`/`-` operators (the WPT cases have no
    // nesting). A leading sign is allowed; operators must be space-separated per CSS calc syntax.
    let mut sign = 1.0f32;
    for (i, tok) in inner.split_whitespace().enumerate() {
        match tok {
            "+" => sign = 1.0,
            "-" => sign = -1.0,
            _ => {
                if i == 0 && tok == "-" {
                    sign = -1.0;
                    continue;
                }
                if let Some(p) = tok.strip_suffix('%').and_then(|n| n.parse::<f32>().ok()) {
                    pct += sign * p;
                    found_pct = true;
                } else if let Some(l) = parse_length_fs(tok, font_size) {
                    px += sign * l;
                } else {
                    return None;
                }
                sign = 1.0;
            }
        }
    }
    if !found_pct {
        return None;
    }
    Some(InsetValue::Calc { pct, px })
}

/// Parse a length for an *edge* (margin/padding/border-width), resolving `em` against `font_size`.
/// Like [`parse_length_fs`] but `auto`/`none` → 0. Unparseable → `None` (leave as-is).
fn parse_edge_length(val: &str, font_size: f32) -> Option<f32> {
    let v = val.trim().to_ascii_lowercase();
    if v == "auto" {
        return Some(0.0); // limitation: margin/padding `auto` collapses to 0
    }
    if v == "none" {
        return Some(0.0);
    }
    parse_length_fs(val, font_size)
}

/// Set one side of an `Edges` from a single length value (ignored if unparseable). `em` resolves
/// against `font_size`.
fn set_edge(edges: &mut Edges, side: EdgeSide, val: &str, font_size: f32) {
    if let Some(px) = parse_edge_length(val, font_size) {
        match side {
            EdgeSide::Top => edges.top = px,
            EdgeSide::Right => edges.right = px,
            EdgeSide::Bottom => edges.bottom = px,
            EdgeSide::Left => edges.left = px,
            EdgeSide::All => *edges = Edges::all(px),
        }
    }
}

/// Parse a `margin`/`padding`/`border-width` shorthand of 1–4 values.
/// CSS order: `all` / `vert horiz` / `top horiz bottom` / `top right bottom left`.
/// Returns `None` if no token parsed (leaves the existing value untouched).
fn parse_edges_shorthand(val: &str, font_size: f32) -> Option<Edges> {
    let parts: Vec<f32> = val
        .split_whitespace()
        .map(|t| parse_edge_length(t, font_size).unwrap_or(0.0))
        .collect();
    match parts.len() {
        1 => Some(Edges::all(parts[0])),
        2 => Some(Edges { top: parts[0], bottom: parts[0], right: parts[1], left: parts[1] }),
        3 => Some(Edges {
            top: parts[0],
            right: parts[1],
            left: parts[1],
            bottom: parts[2],
        }),
        n if n >= 4 => Some(Edges {
            top: parts[0],
            right: parts[1],
            bottom: parts[2],
            left: parts[3],
        }),
        _ => None,
    }
}

/// Apply a `border` (or per-side `border-top` etc.) shorthand: extract a width (the first
/// length token; `none`/`0` → 0) and a color (the first parseable color token). Border style
/// is ignored. Tokens that are neither are skipped.
fn apply_border_shorthand(
    style: &mut ComputedStyle,
    val: &str,
    side: EdgeSide,
    current_color: (u8, u8, u8),
    inherited_color: (u8, u8, u8),
) {
    let mut width: Option<f32> = None;
    let mut color: Option<(u8, u8, u8)> = None;
    let mut saw_none = false;
    for tok in val.split_whitespace() {
        let lower = tok.to_ascii_lowercase();
        if lower == "none" || lower == "hidden" {
            saw_none = true;
            continue;
        }
        // Border-style keywords carry no geometry; skip them.
        if matches!(
            lower.as_str(),
            "solid" | "dashed" | "dotted" | "double" | "groove" | "ridge" | "inset" | "outset"
        ) {
            continue;
        }
        if width.is_none() {
            if let Some(px) = parse_length(tok) {
                width = Some(px);
                continue;
            }
        }
        if color.is_none() {
            if let Some(c) = parse_color_ctx(tok, current_color, inherited_color) {
                color = Some(c);
            }
        }
    }
    let w = if saw_none && width.is_none() { Some(0.0) } else { width };
    if let Some(w) = w {
        match side {
            EdgeSide::Top => style.border.top = w,
            EdgeSide::Right => style.border.right = w,
            EdgeSide::Bottom => style.border.bottom = w,
            EdgeSide::Left => style.border.left = w,
            EdgeSide::All => style.border = Edges::all(w),
        }
    }
    if let Some(c) = color {
        style.border_color = c;
    }
}

/// Parse a `font-weight` value: `bold` / `bolder` / numeric `>= 600` → true; `normal` /
/// `lighter` / numeric `< 600` → false; unknown → `None` (leave inherited).
fn parse_font_weight(val: &str) -> Option<bool> {
    let v = val.trim().to_ascii_lowercase();
    match v.as_str() {
        "bold" | "bolder" => Some(true),
        "normal" | "lighter" => Some(false),
        other => other.parse::<u32>().ok().map(|n| n >= 600),
    }
}

/// Parse a `font-size`: `Npx`, `Npt` (×4/3), or `Nem` (relative to `parent_px`). Bare numbers
/// are treated as px.
fn parse_font_size(val: &str, parent_px: f32) -> Option<f32> {
    let v = val.trim().to_ascii_lowercase();
    // Relative keywords resolve against the parent font size (CSS uses ~1.2× steps).
    match v.as_str() {
        "smaller" => return Some(parent_px / 1.2).filter(|n| *n > 0.0),
        "larger" => return Some(parent_px * 1.2).filter(|n| *n > 0.0),
        _ => {}
    }
    if has_math_func(&v) {
        // `em` in a font-size resolves against the parent font size.
        return eval_length(&v, parent_px).filter(|n| *n > 0.0);
    }
    let num = |suffix: &str| v.strip_suffix(suffix).and_then(|n| n.trim().parse::<f32>().ok());
    if let Some(px) = num("px") {
        Some(px)
    } else if let Some(pt) = num("pt") {
        Some(pt * 4.0 / 3.0)
    } else if let Some(em) = num("em") {
        Some(em * parent_px)
    } else if let Some(rem) = num("rem") {
        Some(rem * 16.0)
    } else if let Some(pct) = num("%") {
        // Percentage font-size is relative to the PARENT's computed font size (e.g. `500%` on the
        // big browserscore.dev score number → 5× its parent).
        Some(pct / 100.0 * parent_px).filter(|n| *n > 0.0)
    } else {
        v.parse::<f32>().ok().filter(|n| *n > 0.0)
    }
}

/// Parse a color to opaque `(r, g, b)`. Supports hex (`#rgb`/`#rgba`/`#rrggbb`/`#rrggbbaa`),
/// named colors, and the functional forms `rgb()`/`rgba()`, `hsl()`/`hsla()`, `oklch()`, and
/// `oklab()`. Alpha is parsed but dropped (treated as opaque). Returns `None` if unrecognized.
///
/// `current` supplies the value of `currentColor`; `inherited` supplies the value used for
/// `inherit`. Keywords `transparent`/`initial` return `None` (caller treats as "no change /
/// no color").
fn parse_color_ctx(
    val: &str,
    current: (u8, u8, u8),
    inherited: (u8, u8, u8),
) -> Option<(u8, u8, u8)> {
    let v = val.trim();
    let lower = v.to_ascii_lowercase();

    // Keywords.
    match lower.as_str() {
        "currentcolor" => return Some(current),
        "inherit" => return Some(inherited),
        "transparent" | "initial" | "unset" | "none" | "revert" => return None,
        _ => {}
    }

    if let Some(hex) = v.strip_prefix('#') {
        return parse_hex(hex);
    }

    // Functional color: name( args ).
    if let Some(open) = v.find('(') {
        if v.ends_with(')') {
            let func = v[..open].trim().to_ascii_lowercase();
            let inner = &v[open + 1..v.len() - 1];
            return parse_color_function(&func, inner);
        }
    }

    parse_named_color(&lower)
}

/// Convenience wrapper used where no element context is needed (currentColor/inherit map to a
/// neutral default). Prefer [`parse_color_ctx`] in the cascade.
#[cfg(test)]
fn parse_color(val: &str) -> Option<(u8, u8, u8)> {
    parse_color_ctx(val, (0, 0, 0), (0, 0, 0))
}

/// Parse a color into [`Rgba`], preserving alpha (unlike [`parse_color_ctx`] which drops it).
/// Handles `transparent` (→ alpha 0), `#rgba`/`#rrggbbaa` hex alpha, and the `/ alpha` or
/// 4th-component alpha of `rgba()`/`hsla()`. `currentColor` resolves to `current` (opaque).
/// Used by gradients and box-shadows where alpha matters. Returns `None` if unrecognized.
fn parse_rgba_ctx(val: &str, current: (u8, u8, u8), inherited: (u8, u8, u8)) -> Option<Rgba> {
    let v = val.trim();
    let lower = v.to_ascii_lowercase();
    match lower.as_str() {
        "currentcolor" => return Some(Rgba { r: current.0, g: current.1, b: current.2, a: 255 }),
        "inherit" => return Some(Rgba { r: inherited.0, g: inherited.1, b: inherited.2, a: 255 }),
        "transparent" => return Some(Rgba { r: 0, g: 0, b: 0, a: 0 }),
        "initial" | "unset" | "none" | "revert" => return None,
        _ => {}
    }
    if let Some(hex) = v.strip_prefix('#') {
        return parse_hex_alpha(hex);
    }
    // Functional form: extract alpha from the args, then defer the rgb to parse_color_function.
    if let Some(open) = v.find('(') {
        if v.ends_with(')') {
            let func = v[..open].trim().to_ascii_lowercase();
            let inner = &v[open + 1..v.len() - 1];
            let (r, g, b) = parse_color_function(&func, inner)?;
            let alpha = parse_func_alpha(inner).unwrap_or(255);
            return Some(Rgba { r, g, b, a: alpha });
        }
    }
    parse_named_color(&lower).map(|(r, g, b)| Rgba { r, g, b, a: 255 })
}

/// Extract the alpha byte from a functional color's argument body (between the parens). Looks for
/// either a `/ <alpha>` segment or a 4th comma/space-separated component. `None` if no alpha.
fn parse_func_alpha(inner: &str) -> Option<u8> {
    // `/ alpha` form (modern syntax).
    if let Some(slash) = inner.split('/').nth(1) {
        return alpha_to_u8(slash.trim());
    }
    // Legacy 4-component form: rgba(r,g,b,a) / hsla(h,s,l,a).
    let toks: Vec<&str> = inner
        .split([',', ' ', '\t', '\n'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if toks.len() >= 4 {
        return alpha_to_u8(toks[3]);
    }
    None
}

/// Parse an alpha token (`0.5`, `50%`, `1`) into a 0..=255 byte.
fn alpha_to_u8(tok: &str) -> Option<u8> {
    let t = tok.trim();
    let f = if let Some(p) = t.strip_suffix('%') {
        p.trim().parse::<f32>().ok()? / 100.0
    } else {
        t.parse::<f32>().ok()?
    };
    Some((f.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// Parse a hex color preserving alpha (`#rgba`/`#rrggbbaa` carry alpha; `#rgb`/`#rrggbb` → opaque).
fn parse_hex_alpha(hex: &str) -> Option<Rgba> {
    let h = hex.trim();
    let hx = |s: &str| u8::from_str_radix(s, 16).ok();
    match h.len() {
        3 => {
            let (r, g, b) = parse_hex(h)?;
            Some(Rgba { r, g, b, a: 255 })
        }
        4 => {
            let (r, g, b) = parse_hex(&h[0..3])?;
            let a = hx(&h[3..4])?;
            Some(Rgba { r, g, b, a: a * 17 })
        }
        6 => {
            let (r, g, b) = parse_hex(h)?;
            Some(Rgba { r, g, b, a: 255 })
        }
        8 => {
            let (r, g, b) = parse_hex(&h[0..6])?;
            let a = hx(&h[6..8])?;
            Some(Rgba { r, g, b, a })
        }
        _ => None,
    }
}

/// Parse a functional color body (the text between the parens), given the lowercased function
/// name. Handles `rgb`/`rgba`/`hsl`/`hsla`/`oklch`/`oklab`.
fn parse_color_function(func: &str, inner: &str) -> Option<(u8, u8, u8)> {
    // Relative-color syntax (`rgb(from red r g b)`) and other exotic forms are not supported;
    // bail out so the caller can fall back rather than mis-parse.
    if inner.trim_start().to_ascii_lowercase().starts_with("from ") {
        return None;
    }
    // Split on commas and/or whitespace; also strip an optional `/ alpha` segment.
    let main = inner.split('/').next().unwrap_or(inner);
    let toks: Vec<&str> = main
        .split([',', ' ', '\t', '\n'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    match func {
        "rgb" | "rgba" => {
            if toks.len() < 3 {
                return None;
            }
            Some((
                parse_rgb_component(toks[0])?,
                parse_rgb_component(toks[1])?,
                parse_rgb_component(toks[2])?,
            ))
        }
        "hsl" | "hsla" => {
            if toks.len() < 3 {
                return None;
            }
            let h = parse_number(toks[0])?;
            let s = parse_percent_or_unit(toks[1])?; // 0..1
            let l = parse_percent_or_unit(toks[2])?; // 0..1
            Some(hsl_to_rgb(h, s, l))
        }
        "oklch" => {
            if toks.len() < 3 {
                return None;
            }
            let l = parse_percent_or_unit(toks[0])?; // 0..1 (or %)
            let c = parse_number(toks[1])?;
            let h = parse_number(toks[2])?;
            Some(oklch_to_srgb(l, c, h))
        }
        "oklab" => {
            if toks.len() < 3 {
                return None;
            }
            let l = parse_percent_or_unit(toks[0])?;
            let a = parse_number(toks[1])?;
            let b = parse_number(toks[2])?;
            Some(oklab_to_srgb(l, a, b))
        }
        _ => None,
    }
}

/// Parse an rgb channel: `0..255` integer/float, or a percentage `0%..100%`.
fn parse_rgb_component(tok: &str) -> Option<u8> {
    if let Some(p) = tok.strip_suffix('%') {
        let pct = p.trim().parse::<f32>().ok()?;
        return Some((pct / 100.0 * 255.0).round().clamp(0.0, 255.0) as u8);
    }
    let n = tok.parse::<f32>().ok()?;
    Some(n.round().clamp(0.0, 255.0) as u8)
}

/// Parse a bare number (drops a trailing `deg`/`rad`/`turn` unit on angles, treating the value
/// as already in the natural unit for the caller — degrees for hue, etc.).
fn parse_number(tok: &str) -> Option<f32> {
    let t = tok.trim();
    for unit in ["deg", "grad", "rad", "turn"] {
        if let Some(stripped) = t.strip_suffix(unit) {
            let v = stripped.trim().parse::<f32>().ok()?;
            return Some(match unit {
                "deg" => v,
                "grad" => v * 0.9,
                "rad" => v.to_degrees(),
                "turn" => v * 360.0,
                _ => v,
            });
        }
    }
    t.parse::<f32>().ok()
}

/// Parse a value that may be a percentage (`50%` → 0.5) or a unitless number used as-is.
/// Parse a bare `<percentage>` token (`"50%"` → `Some(50.0)`); `None` for anything else.
fn parse_percent(val: &str) -> Option<f32> {
    val.trim().strip_suffix('%').and_then(|p| p.trim().parse::<f32>().ok())
}

fn parse_percent_or_unit(tok: &str) -> Option<f32> {
    if let Some(p) = tok.strip_suffix('%') {
        return p.trim().parse::<f32>().ok().map(|v| v / 100.0);
    }
    tok.trim().parse::<f32>().ok()
}

/// HSL (h in degrees, s/l in 0..1) → sRGB 8-bit.
fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let h = ((h % 360.0) + 360.0) % 360.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (((h / 60.0) % 2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r1, g1, b1) = match (h / 60.0) as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (
        (((r1 + m) * 255.0).round()).clamp(0.0, 255.0) as u8,
        (((g1 + m) * 255.0).round()).clamp(0.0, 255.0) as u8,
        (((b1 + m) * 255.0).round()).clamp(0.0, 255.0) as u8,
    )
}

/// OKLCH (L 0..1, C chroma, H degrees) → sRGB 8-bit.
fn oklch_to_srgb(l: f32, c: f32, h: f32) -> (u8, u8, u8) {
    let hr = h.to_radians();
    oklab_to_srgb(l, c * hr.cos(), c * hr.sin())
}

/// OKLab (L 0..1, a, b) → sRGB 8-bit. Uses the standard OKLab→linear-sRGB matrices, then the
/// sRGB transfer function, clamped to [0, 255].
fn oklab_to_srgb(l: f32, a: f32, b: f32) -> (u8, u8, u8) {
    // OKLab → LMS' (cube of intermediate).
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;

    let lc = l_ * l_ * l_;
    let mc = m_ * m_ * m_;
    let sc = s_ * s_ * s_;

    // LMS → linear sRGB.
    let lr = 4.076_741_7 * lc - 3.307_711_6 * mc + 0.230_969_94 * sc;
    let lg = -1.268_438 * lc + 2.609_757_4 * mc - 0.341_319_38 * sc;
    let lb = -0.004_196_086 * lc - 0.703_418_6 * mc + 1.707_614_7 * sc;

    (
        srgb_encode(lr),
        srgb_encode(lg),
        srgb_encode(lb),
    )
}

/// Linear sRGB component (0..1, may be out of range) → gamma-encoded 8-bit, clamped.
fn srgb_encode(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let v = if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (v * 255.0).round().clamp(0.0, 255.0) as u8
}

fn parse_named_color(lower: &str) -> Option<(u8, u8, u8)> {
    let named = match lower {
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "green" => (0, 128, 0),
        "lime" => (0, 255, 0),
        "blue" => (0, 0, 255),
        "gray" | "grey" => (128, 128, 128),
        "silver" => (192, 192, 192),
        "yellow" => (255, 255, 0),
        "orange" => (255, 165, 0),
        "purple" => (128, 0, 128),
        "cyan" | "aqua" => (0, 255, 255),
        "magenta" | "fuchsia" => (255, 0, 255),
        "maroon" => (128, 0, 0),
        "navy" => (0, 0, 128),
        "teal" => (0, 128, 128),
        "olive" => (128, 128, 0),
        "pink" => (255, 192, 203),
        "brown" => (165, 42, 42),
        _ => return None,
    };
    Some(named)
}

fn parse_hex(hex: &str) -> Option<(u8, u8, u8)> {
    let h = hex.trim();
    let hx = |s: &str| u8::from_str_radix(s, 16).ok();
    match h.len() {
        // #rgb
        3 => {
            let r = hx(&h[0..1])?;
            let g = hx(&h[1..2])?;
            let b = hx(&h[2..3])?;
            Some((r * 17, g * 17, b * 17))
        }
        // #rgba — drop alpha.
        4 => {
            let r = hx(&h[0..1])?;
            let g = hx(&h[1..2])?;
            let b = hx(&h[2..3])?;
            Some((r * 17, g * 17, b * 17))
        }
        // #rrggbb
        6 => {
            let r = hx(&h[0..2])?;
            let g = hx(&h[2..4])?;
            let b = hx(&h[4..6])?;
            Some((r, g, b))
        }
        // #rrggbbaa — drop alpha.
        8 => {
            let r = hx(&h[0..2])?;
            let g = hx(&h[2..4])?;
            let b = hx(&h[4..6])?;
            Some((r, g, b))
        }
        _ => None,
    }
}

/// If any selector in `selectors` matches `el`, return the highest specificity among the
/// matching ones (encoded as id*100 + class*10 + type). `None` if none match.
///
/// The cascade now matches via [`SelectorIndex`] rather than calling this per rule, but it is
/// retained as the reference single-rule matcher (used by tests / external callers).
#[allow(dead_code)]
fn rule_specificity(selectors: &[String], doc: &dom::Document, id: dom::NodeId) -> Option<u32> {
    let mut best: Option<u32> = None;
    for sel in selectors {
        if let Some(c) = compile_selector(sel) {
            if complex_matches(doc, id, &c.selector) {
                best = Some(best.map_or(c.specificity, |b| b.max(c.specificity)));
            }
        }
    }
    best
}

// ===========================================================================================
// Complex selector engine
// ===========================================================================================
//
// A *complex* selector is a sequence of compound selectors joined by combinators, evaluated
// right-to-left. We parse each selector string into a [`ComplexSelector`] (a `Vec` of
// `(Combinator, Compound)` stored RIGHTMOST-FIRST) and match it against a `(doc, node_id)`
// pair by walking the appropriate tree axis for each combinator, with backtracking for the
// descendant / general-sibling axes.
//
// Pseudo-ELEMENTS (`::before`, `::after`, `::placeholder`, `::marker`) are OUT OF SCOPE: a
// selector containing one is treated as non-matching (its parse returns `None`, so the rule is
// dropped from the index) — we never crash on it.

/// How a compound relates to the compound on its right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Combinator {
    /// Rightmost compound (no left neighbor) — the "subject" of the selector.
    Subject,
    /// Descendant (whitespace): some ancestor matches.
    Descendant,
    /// Child (`>`): the parent matches.
    Child,
    /// Adjacent sibling (`+`): the immediately-preceding element sibling matches.
    NextSibling,
    /// General sibling (`~`): some preceding element sibling matches.
    SubsequentSibling,
}

/// One attribute selector `[name OP value FLAG]`.
#[derive(Debug, Clone)]
struct AttrSel {
    name: String,
    op: AttrOp,
    value: String,
    case_insensitive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttrOp {
    /// `[attr]` — present.
    Exists,
    /// `[attr=val]`.
    Equals,
    /// `[attr~=val]` — whitespace-separated word.
    Includes,
    /// `[attr|=val]` — val or val-`-`….
    DashMatch,
    /// `[attr^=val]` — prefix.
    Prefix,
    /// `[attr$=val]` — suffix.
    Suffix,
    /// `[attr*=val]` — substring.
    Substring,
}

/// A pseudo-class. Structural ones need tree/sibling position; state ones consult interaction
/// state or element attributes; functional ones recurse into nested selector lists.
#[derive(Debug, Clone)]
enum Pseudo {
    // Structural
    FirstChild,
    LastChild,
    OnlyChild,
    FirstOfType,
    LastOfType,
    OnlyOfType,
    NthChild(NthArg),
    NthLastChild(NthArg),
    NthOfType(NthArg),
    NthLastOfType(NthArg),
    Root,
    Empty,
    // State (attribute-derived)
    Checked,
    Disabled,
    Enabled,
    Required,
    Optional,
    Link,    // <a href> (also :any-link / :visited→never matches, see parse)
    // State (interaction)
    Hover,
    Focus,
    Active,
    FocusWithin,
    FocusVisible,
    /// `:lang(<ident>)` — matches if the element's (inherited) `lang` equals the argument or begins
    /// with `<arg>-`. The argument is stored lowercased.
    Lang(String),
    // Functional
    Not(Vec<ComplexSelector>),
    Is(Vec<ComplexSelector>),
    Where(Vec<ComplexSelector>),
    /// Recognized-but-never-matches (`:visited`, `:target`, `:default`, `:placeholder-shown`,
    /// etc.) — best-effort: parses cleanly so the rest of the selector still works, but the
    /// element never matches it.
    NeverMatch,
}

/// An `An+B` argument for `:nth-*`.
#[derive(Debug, Clone, Copy)]
struct NthArg {
    a: i32,
    b: i32,
}

impl NthArg {
    /// Does a 1-based index `n` satisfy `An+B`? i.e. exists k>=0 with n == a*k + b.
    fn matches(&self, n: i32) -> bool {
        if self.a == 0 {
            return n == self.b;
        }
        let diff = n - self.b;
        diff % self.a == 0 && diff / self.a >= 0
    }
}

/// The namespace component of a type/universal selector (`svg|`, `*|`, `|`, or none).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum NsConstraint {
    /// No namespace prefix written. With a declared default `@namespace`, the type is constrained to
    /// it; otherwise (no default) it matches any namespace.
    #[default]
    Unspecified,
    /// `*|` — explicitly any namespace.
    Any,
    /// `|` — explicitly the null namespace (no namespace).
    None,
    /// `prefix|` — the namespace bound to `prefix` by an `@namespace` rule.
    Prefixed(String),
}

/// A compound selector: an optional type plus any number of class/id/attr/pseudo simples.
#[derive(Debug, Clone, Default)]
struct Compound {
    /// Leading type, lowercased. `None` = universal (`*`) or no explicit type.
    type_part: Option<String>,
    /// The namespace component of the (possibly implied universal) type selector.
    type_ns: NsConstraint,
    classes: Vec<String>,
    ids: Vec<String>,
    attrs: Vec<AttrSel>,
    pseudos: Vec<Pseudo>,
}

/// A full complex selector, stored rightmost-compound-first. `parts[0]` is the subject.
#[derive(Debug, Clone)]
struct ComplexSelector {
    parts: Vec<(Combinator, Compound)>,
    specificity: u32,
    /// A trailing `::before`/`::after` (or legacy `:before`/`:after`) on the subject compound.
    /// `None` for an ordinary element selector.
    pseudo_element: Option<PseudoElement>,
}

/// What we bucket a compiled selector under in the [`SelectorIndex`]: the most-selective simple
/// part of the RIGHTMOST (subject) compound.
#[derive(Debug, Clone)]
enum BucketKey {
    Id(String),
    Class(String),
    Type(String),
    Universal,
}

/// A CSS pseudo-element. `Before`/`After` generate content boxes during layout; everything else is
/// modeled as `Other(key)` so author rules targeting it can still match (for `getComputedStyle`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PseudoElement {
    Before,
    After,
    /// Any other pseudo-element (`::marker`, `::placeholder`, `::highlight(name)`,
    /// `::picker(select)`, …), stored as its normalized key (lowercased name, plus a normalized
    /// `(arg)` for functional pseudos). These don't generate layout boxes here, but they DO match
    /// author rules so `getComputedStyle(el, "::marker")` can return the pseudo's cascaded style.
    Other(String),
}

impl PseudoElement {
    /// The canonical key two selectors / a getComputedStyle arg compare equal on.
    pub fn key(&self) -> String {
        match self {
            PseudoElement::Before => "before".to_string(),
            PseudoElement::After => "after".to_string(),
            PseudoElement::Other(k) => k.clone(),
        }
    }
}

/// A compiled selector ready for the index: the parsed [`ComplexSelector`] plus its bucket key.
#[derive(Debug, Clone)]
struct Compiled {
    selector: ComplexSelector,
    key: BucketKey,
    specificity: u32,
    /// `Some` if this selector targets a `::before`/`::after` pseudo-element. The rest of the
    /// selector still matches the ORIGINATING element normally; this just routes the result.
    pseudo_element: Option<PseudoElement>,
}

impl Compiled {
    fn bucket_key(&self) -> &BucketKey {
        &self.key
    }
}

/// Specificity weights packed into a sortable u32 (a*10000 + b*100 + c), matching the existing
/// scheme's magnitude (id=100, class=10, type=1 historically; the new packing keeps the same
/// relative ordering with more headroom for many components).
#[derive(Debug, Clone, Copy, Default)]
struct Spec {
    a: u32, // ids
    b: u32, // classes / attrs / pseudo-classes
    c: u32, // types / pseudo-elements
}

impl Spec {
    fn pack(&self) -> u32 {
        self.a.min(9999) * 10000 + self.b.min(99) * 100 + self.c.min(99)
    }
    fn add(&mut self, o: Spec) {
        self.a += o.a;
        self.b += o.b;
        self.c += o.c;
    }
    fn max_with(self, o: Spec) -> Spec {
        if (self.a, self.b, self.c) >= (o.a, o.b, o.c) { self } else { o }
    }
}

/// Parse one (possibly complex) selector string into a [`Compiled`], or `None` if it uses
/// syntax we never match — chiefly pseudo-ELEMENTS (`::before`) or malformed input. This is the
/// single source of truth for selector parsing used by the cascade index.
fn compile_selector(sel: &str) -> Option<Compiled> {
    let selector = parse_complex(sel)?;
    let specificity = selector.specificity;
    // Bucket key = most-selective simple part of the rightmost (subject) compound.
    let subject = &selector.parts[0].1;
    let key = if let Some(id) = subject.ids.first() {
        BucketKey::Id(id.clone())
    } else if let Some(class) = subject.classes.first() {
        BucketKey::Class(class.clone())
    } else if let Some(t) = &subject.type_part {
        BucketKey::Type(t.clone())
    } else {
        // Purely `[attr]`/`:pseudo`/`*` subject → universal bucket.
        BucketKey::Universal
    };
    let pseudo_element = selector.pseudo_element.clone();
    Some(Compiled { selector, key, specificity, pseudo_element })
}

/// Parse a complex selector into rightmost-first `(Combinator, Compound)` parts, computing its
/// specificity. Returns `None` if any compound fails to parse (e.g. a pseudo-element).
fn parse_complex(sel: &str) -> Option<ComplexSelector> {
    let chars: Vec<char> = sel.trim().chars().collect();
    if chars.is_empty() {
        return None;
    }
    // Tokenize into (combinator-to-the-left, compound-text) pairs, left-to-right, then reverse.
    // We split on top-level whitespace / `>` / `+` / `~` (not inside [], (), or quotes).
    let mut parts: Vec<(Combinator, String)> = Vec::new();
    let mut cur = String::new();
    // Combinator that precedes the NEXT compound to be flushed (relates it to the PREVIOUS
    // compound). The first compound has no preceding combinator (`Subject`).
    let mut pending_comb = Combinator::Subject;
    let mut i = 0;
    let mut depth_brk = 0i32; // []
    let mut depth_par = 0i32; // ()
    let mut quote: Option<char> = None;
    let n = chars.len();
    // Flush the current compound text, tagged with the pending combinator; reset pending to the
    // "no combinator seen yet" sentinel for the next compound.
    let flush = |cur: &mut String, pending: &mut Combinator, parts: &mut Vec<(Combinator, String)>| {
        if !cur.is_empty() {
            parts.push((*pending, std::mem::take(cur)));
            *pending = Combinator::Subject; // sentinel; overwritten before next flush
        }
    };
    while i < n {
        let c = chars[i];
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            '[' => {
                depth_brk += 1;
                cur.push(c);
            }
            ']' => {
                depth_brk -= 1;
                cur.push(c);
            }
            '(' => {
                depth_par += 1;
                cur.push(c);
            }
            ')' => {
                depth_par -= 1;
                cur.push(c);
            }
            _ if depth_brk > 0 || depth_par > 0 => cur.push(c),
            c if c.is_whitespace() => {
                // Whitespace: flush the current compound, then tentatively mark the next
                // combinator as descendant. An explicit combinator immediately after overrides
                // it (so `.a > .b` parses Child, not Descendant).
                flush(&mut cur, &mut pending_comb, &mut parts);
                pending_comb = Combinator::Descendant;
                while i + 1 < n && chars[i + 1].is_whitespace() {
                    i += 1;
                }
            }
            '>' | '+' | '~' => {
                // Explicit combinator relates the NEXT compound to the previous one. Flush the
                // current compound first (handles the no-whitespace case `a>b`).
                flush(&mut cur, &mut pending_comb, &mut parts);
                pending_comb = match c {
                    '>' => Combinator::Child,
                    '+' => Combinator::NextSibling,
                    _ => Combinator::SubsequentSibling,
                };
                while i + 1 < n && chars[i + 1].is_whitespace() {
                    i += 1;
                }
            }
            _ => cur.push(c),
        }
        i += 1;
    }
    flush(&mut cur, &mut pending_comb, &mut parts);
    if parts.is_empty() {
        return None;
    }
    // `parts[i].0` is the combinator BEFORE compound i (linking it to compound i-1) in source
    // order. We want each compound to carry the combinator linking it to its RIGHT neighbor:
    //   right_link[i] = before[i+1]   (and the last compound's right_link = Subject).
    // Then reversing puts the subject at index 0, and every `match_from` step at index `idx`
    // reads the combinator relating `parts[idx]` to `parts[idx-1]` (its source-right neighbor).
    let k = parts.len();
    let mut right_link: Vec<Combinator> = Vec::with_capacity(k);
    for i in 0..k {
        if i + 1 < k {
            right_link.push(parts[i + 1].0);
        } else {
            right_link.push(Combinator::Subject);
        }
    }

    let mut out: Vec<(Combinator, Compound)> = Vec::with_capacity(k);
    let mut spec = Spec::default();
    let mut pseudo_element = None;
    // Build rightmost-first. Only the rightmost (subject, source-last) compound may carry a
    // trailing `::before`/`::after`.
    for i in (0..k).rev() {
        let is_subject = i == k - 1;
        let (compound, cspec, pe) = parse_compound(&parts[i].1)?;
        // A pseudo-element is only valid on the subject; anywhere else it's malformed.
        if pe.is_some() && !is_subject {
            return None;
        }
        if is_subject {
            pseudo_element = pe;
        }
        spec.add(cspec);
        out.push((right_link[i], compound));
    }
    Some(ComplexSelector { parts: out, specificity: spec.pack(), pseudo_element })
}

/// Parse a single compound selector (`type.class#id[attr]:pseudo`...). Returns the compound and
/// its specificity, or `None` on a pseudo-element / malformed token.
fn parse_compound(text: &str) -> Option<(Compound, Spec, Option<PseudoElement>)> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut compound = Compound::default();
    let mut spec = Spec::default();
    // Set when we strip a trailing `::before`/`::after` (or legacy single-colon). The pseudo-element
    // must be the LAST token in the compound, so once seen, nothing else may follow.
    let mut pseudo_element: Option<PseudoElement> = None;

    // Optional leading namespace prefix + type / universal. A `|` may appear after an ident, a `*`,
    // or at the very start (`|tag` = null namespace). Recognize: `prefix|tag`, `prefix|*`, `*|tag`,
    // `*|*`, `|tag`, `|*`, plain `tag`, plain `*`.
    // First, read an optional prefix token (`ident`, `*`, or empty) that is immediately followed by
    // a `|` (but not `||`, the column combinator).
    let has_ns_pipe = {
        // Scan a tentative prefix: empty, `*`, or an ident, then require a single `|` not followed
        // by another `|`.
        let mut k = i;
        if k < n && chars[k] == '*' {
            k += 1;
        } else {
            while k < n && (chars[k].is_alphanumeric() || chars[k] == '-' || chars[k] == '_') {
                k += 1;
            }
        }
        k < n && chars[k] == '|' && !(k + 1 < n && chars[k + 1] == '|')
    };
    if has_ns_pipe {
        let pstart = i;
        if i < n && chars[i] == '*' {
            i += 1;
            compound.type_ns = NsConstraint::Any;
        } else {
            while i < n && (chars[i].is_alphanumeric() || chars[i] == '-' || chars[i] == '_') {
                i += 1;
            }
            let prefix: String = chars[pstart..i].iter().collect();
            compound.type_ns = if prefix.is_empty() {
                NsConstraint::None
            } else {
                NsConstraint::Prefixed(prefix)
            };
        }
        i += 1; // consume the `|`
        // Now read the type / universal that follows the namespace prefix.
        if i < n && chars[i] == '*' {
            i += 1; // universal within this namespace
        } else if i < n && !matches!(chars[i], '.' | '#' | '[' | ':') {
            let start = i;
            while i < n && !matches!(chars[i], '.' | '#' | '[' | ':' | '*') {
                i += 1;
            }
            let t: String = chars[start..i].iter().collect();
            if !is_ident(&t) {
                return None;
            }
            compound.type_part = Some(t.to_lowercase());
            spec.c += 1;
        } else {
            return None; // `ns|` with no type/universal is invalid
        }
    } else if i < n && chars[i] != '.' && chars[i] != '#' && chars[i] != '[' && chars[i] != ':' && chars[i] != '*' {
        let start = i;
        while i < n && !matches!(chars[i], '.' | '#' | '[' | ':' | '*') {
            i += 1;
        }
        let t: String = chars[start..i].iter().collect();
        if !is_ident(&t) {
            return None;
        }
        compound.type_part = Some(t.to_lowercase());
        spec.c += 1;
    } else if i < n && chars[i] == '*' {
        i += 1; // universal, no specificity, no type constraint
    }

    while i < n {
        match chars[i] {
            '.' => {
                i += 1;
                let start = i;
                while i < n && is_name_char(chars[i]) {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                if name.is_empty() {
                    return None;
                }
                compound.classes.push(name);
                spec.b += 1;
            }
            '#' => {
                i += 1;
                let start = i;
                while i < n && is_name_char(chars[i]) {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                if name.is_empty() {
                    return None;
                }
                compound.ids.push(name);
                spec.a += 1;
            }
            '[' => {
                // Read up to the matching ']' (no nested brackets in attribute selectors).
                let start = i + 1;
                let mut j = start;
                let mut quote: Option<char> = None;
                while j < n {
                    let c = chars[j];
                    if let Some(q) = quote {
                        if c == q {
                            quote = None;
                        }
                    } else if c == '"' || c == '\'' {
                        quote = Some(c);
                    } else if c == ']' {
                        break;
                    }
                    j += 1;
                }
                if j >= n {
                    return None; // unterminated
                }
                let inner: String = chars[start..j].iter().collect();
                let attr = parse_attr(&inner)?;
                compound.attrs.push(attr);
                spec.b += 1;
                i = j + 1;
            }
            ':' => {
                // A pseudo-element uses double-colon (`::before`) syntax; legacy CSS2 also allowed
                // single-colon (`:before`). Detect either and, for the two we support, strip them
                // (routing the result to the element's ::before/::after style); a pseudo-element
                // must be the rightmost token, so nothing may follow it.
                let double_colon = i + 1 < n && chars[i + 1] == ':';
                let name_start = if double_colon { i + 2 } else { i + 1 };
                let mut j = name_start;
                while j < n && is_name_char(chars[j]) {
                    j += 1;
                }
                let pe_name: String = chars[name_start..j].iter().collect();
                let pe_name_l = pe_name.to_ascii_lowercase();
                // A pseudo-element may be functional (`::highlight(name)`, `::picker(select)`); its
                // `(arg)` is part of the pseudo-element token.
                let mut after_pe = j;
                let pe_arg: Option<String> = if after_pe < n && chars[after_pe] == '(' {
                    let astart = after_pe + 1;
                    let mut depth = 1i32;
                    let mut k = astart;
                    while k < n && depth > 0 {
                        match chars[k] {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        k += 1;
                    }
                    if k >= n {
                        return None;
                    }
                    let a: String = chars[astart..k].iter().collect();
                    after_pe = k + 1;
                    Some(a)
                } else {
                    None
                };
                // `::before` / `:before` and `::after` / `:after` are the box-generating pseudos.
                // Single-colon is legacy CSS2 syntax, valid only for the four original pseudo-
                // elements; every other pseudo-element requires double-colon.
                let legacy_single = matches!(pe_name_l.as_str(), "before" | "after" | "first-line" | "first-letter");
                let known_pe = if !double_colon && !legacy_single {
                    None
                } else {
                    match pe_name_l.as_str() {
                        "before" if pe_arg.is_none() => Some(PseudoElement::Before),
                        "after" if pe_arg.is_none() => Some(PseudoElement::After),
                        _ => pseudo_element_key(&pe_name_l, pe_arg.as_deref()).map(PseudoElement::Other),
                    }
                };
                if let Some(pe) = known_pe {
                    // A pseudo-element must be the rightmost token in the compound.
                    if after_pe != n {
                        return None;
                    }
                    pseudo_element = Some(pe);
                    // A pseudo-element contributes one type-level (c) specificity unit.
                    spec.c += 1;
                    i = after_pe;
                    continue;
                }
                // Double-colon syntax is *only* for pseudo-elements; an unrecognized one is invalid.
                if double_colon {
                    return None;
                }
                i += 1;
                let start = i;
                while i < n && is_name_char(chars[i]) {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                let name_l = name.to_ascii_lowercase();
                // Functional pseudo with `(...)`.
                let arg = if i < n && chars[i] == '(' {
                    let astart = i + 1;
                    let mut depth = 1i32;
                    let mut j = astart;
                    let mut quote: Option<char> = None;
                    while j < n && depth > 0 {
                        let c = chars[j];
                        if let Some(q) = quote {
                            if c == q {
                                quote = None;
                            }
                        } else if c == '"' || c == '\'' {
                            quote = Some(c);
                        } else if c == '(' {
                            depth += 1;
                        } else if c == ')' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        j += 1;
                    }
                    if j >= n {
                        return None;
                    }
                    let a: String = chars[astart..j].iter().collect();
                    i = j + 1;
                    Some(a)
                } else {
                    None
                };
                let (pseudo, pspec) = parse_pseudo(&name_l, arg.as_deref())?;
                spec.add(pspec);
                compound.pseudos.push(pseudo);
            }
            '*' => return None, // universal not allowed mid-compound
            _ => return None,
        }
    }
    Some((compound, spec, pseudo_element))
}

/// Validate and normalize a pseudo-element `name` (already lowercased) plus its optional functional
/// `arg` into a canonical key. Returns `None` for unrecognized pseudo-elements or malformed args.
/// `before`/`after` are handled by the caller as their own enum variants; this covers the rest.
///
/// The key is the lowercased name, plus `(arg)` for functional pseudos where `arg` is the
/// normalized (unescaped, lowercased ident) argument. The accepted set mirrors the WPT corpus.
fn pseudo_element_key(name: &str, arg: Option<&str>) -> Option<String> {
    // Functional pseudo-elements: name -> validator for the argument.
    //   ::highlight(<ident>), ::view-transition-*(<ident>|*), ::picker(<ident>)
    let functional: &[&str] = &[
        "highlight",
        "view-transition-group",
        "view-transition-image-pair",
        "view-transition-old",
        "view-transition-new",
        "picker",
    ];
    // Tree-structural / plain pseudo-elements (no argument).
    let plain: &[&str] = &[
        "first-line",
        "first-letter",
        "marker",
        "placeholder",
        "selection",
        "backdrop",
        "file-selector-button",
        "grammar-error",
        "spelling-error",
        "target-text",
        "view-transition",
        "checkmark",
        "picker-icon",
    ];

    match arg {
        Some(raw) => {
            if !functional.contains(&name) {
                return None; // a non-functional pseudo got an argument → invalid
            }
            // The argument must be a single CSS identifier. Surrounding whitespace is allowed;
            // escapes are decoded. (`*` is NOT accepted for view-transition-* in getComputedStyle.)
            let trimmed = raw.trim_matches(|c: char| c.is_ascii_whitespace());
            let ident = decode_css_ident(trimmed).filter(|s| is_css_ident(s))?;
            // `::picker(...)` only accepts the literal `select` keyword as its argument.
            if name == "picker" && ident.to_ascii_lowercase() != "select" {
                return None;
            }
            Some(format!("{name}({})", ident.to_lowercase()))
        }
        None => {
            if plain.contains(&name) {
                Some(name.to_string())
            } else {
                None // functional pseudo without an argument, or unknown name
            }
        }
    }
}

/// Whether `s` is a valid CSS identifier: non-empty, may not start with a digit (nor `-` followed
/// by a digit), and contains only name characters. Used to validate pseudo-element arguments.
fn is_css_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else { return false };
    let valid_start = |c: char| c.is_ascii_alphabetic() || c == '_' || c == '-' || !c.is_ascii();
    if !valid_start(first) {
        return false;
    }
    // `-` alone, or `-` followed by a digit, is not a valid identifier start.
    if first == '-' {
        match s.chars().nth(1) {
            None => return false,
            Some(c) if c.is_ascii_digit() => return false,
            _ => {}
        }
    }
    s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || !c.is_ascii())
}

/// Decode a CSS identifier that may contain escapes (`\61`, `\ `, …). Returns `None` if the input
/// isn't a valid identifier (contains a raw delimiter, etc.). Used to normalize pseudo-element
/// names and functional arguments coming from `getComputedStyle`'s string argument.
fn decode_css_ident(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            // CSS escape: either a hex sequence (1-6 hex digits, optional trailing whitespace) or
            // a literal escaped character.
            if i + 1 >= chars.len() {
                return None; // trailing backslash
            }
            let next = chars[i + 1];
            if next.is_ascii_hexdigit() {
                let mut hex = String::new();
                let mut k = i + 1;
                while k < chars.len() && hex.len() < 6 && chars[k].is_ascii_hexdigit() {
                    hex.push(chars[k]);
                    k += 1;
                }
                // One optional whitespace terminates the hex escape.
                if k < chars.len() && chars[k].is_ascii_whitespace() {
                    k += 1;
                }
                let cp = u32::from_str_radix(&hex, 16).ok()?;
                out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                i = k;
            } else {
                out.push(next);
                i += 2;
            }
        } else if c.is_ascii_alphanumeric() || c == '-' || c == '_' || !c.is_ascii() {
            out.push(c);
            i += 1;
        } else {
            return None; // raw delimiter (space, comma, paren, …) is not part of an identifier
        }
    }
    Some(out)
}

/// The result of normalizing `getComputedStyle`'s second (`pseudoElt`) argument per CSSOM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GcsPseudo {
    /// No pseudo (empty / null / a token that doesn't start with `:`) — use the element's own style.
    Element,
    /// A valid, recognized pseudo-element. Carries the canonical key (`"before"`, `"highlight(x)"`).
    Pseudo(String),
    /// A syntactically-valid-looking but unrecognized/invalid pseudo — yields an empty style.
    Invalid,
}

/// Normalize the `pseudoElt` argument of `getComputedStyle(elt, pseudoElt)` per the CSSOM
/// "legacy pseudo-element parsing" rules:
///   - empty / no leading `:` → ignore (use the element).
///   - one or two leading colons + a valid pseudo-element identifier (and nothing else) → that
///     pseudo-element; single-colon is legacy and only valid for before/after/first-line/first-letter.
///   - anything else (trailing tokens, unknown identifier, double-colon-required pseudos with a
///     single colon, malformed functional args) → invalid (empty style).
pub fn parse_gcs_pseudo(arg: &str) -> GcsPseudo {
    let chars: Vec<char> = arg.chars().collect();
    let n = chars.len();
    if n == 0 || chars[0] != ':' {
        return GcsPseudo::Element;
    }
    let double = n >= 2 && chars[1] == ':';
    let name_start = if double { 2 } else { 1 };
    // Read the identifier (name chars, including escapes — a backslash escapes the next run).
    let mut i = name_start;
    while i < n {
        let c = chars[i];
        if c == '\\' {
            // Consume the escape (hex run or single char) as part of the ident token.
            i += 1;
            if i < n && chars[i].is_ascii_hexdigit() {
                let mut len = 0;
                while i < n && len < 6 && chars[i].is_ascii_hexdigit() {
                    i += 1;
                    len += 1;
                }
                if i < n && chars[i].is_ascii_whitespace() {
                    i += 1;
                }
            } else if i < n {
                i += 1;
            }
        } else if c.is_ascii_alphanumeric() || c == '-' || c == '_' || !c.is_ascii() {
            i += 1;
        } else {
            break;
        }
    }
    let name_raw: String = chars[name_start..i].iter().collect();
    let Some(name) = decode_css_ident(&name_raw) else {
        return GcsPseudo::Invalid;
    };
    let name_l = name.to_ascii_lowercase();

    // Optional functional argument. Per the CSSOM legacy-pseudo grammar, an unterminated `(` is
    // tolerated (auto-closed at end of input): `::highlight(\nname` parses like `::highlight(name)`.
    let arg_opt: Option<String> = if i < n && chars[i] == '(' {
        let astart = i + 1;
        let mut k = astart;
        while k < n && chars[k] != ')' {
            k += 1;
        }
        let a: String = chars[astart..k].iter().collect();
        // Consume the `)` if present; otherwise we're at end of input (auto-closed).
        i = if k < n { k + 1 } else { k };
        Some(a)
    } else {
        None
    };

    // Nothing (except trailing whitespace? no — CSSOM forbids trailing tokens) may follow.
    if i != n {
        return GcsPseudo::Invalid;
    }

    // before/after (both colon forms) and the legacy four (single colon ok); everything else needs
    // double colon.
    let legacy_single = matches!(name_l.as_str(), "before" | "after" | "first-line" | "first-letter");
    if !double && !legacy_single {
        return GcsPseudo::Invalid;
    }
    match name_l.as_str() {
        "before" if arg_opt.is_none() => GcsPseudo::Pseudo("before".to_string()),
        "after" if arg_opt.is_none() => GcsPseudo::Pseudo("after".to_string()),
        _ => match pseudo_element_key(&name_l, arg_opt.as_deref()) {
            Some(key) => GcsPseudo::Pseudo(key),
            None => GcsPseudo::Invalid,
        },
    }
}

/// Parse the inside of `[...]` into an [`AttrSel`].
fn parse_attr(inner: &str) -> Option<AttrSel> {
    let s = inner.trim();
    // Detect a trailing ` i` / ` s` case flag (only meaningful with a value, but tolerate it).
    let mut case_insensitive = false;
    let mut body = s.to_string();
    {
        let lower = body.to_ascii_lowercase();
        if lower.ends_with(" i") {
            case_insensitive = true;
            let len = body.len() - 2;
            body.truncate(len);
            body = body.trim_end().to_string();
        } else if lower.ends_with(" s") {
            let len = body.len() - 2;
            body.truncate(len);
            body = body.trim_end().to_string();
        }
    }
    // Find the operator.
    let ops: [(&str, AttrOp); 6] = [
        ("~=", AttrOp::Includes),
        ("|=", AttrOp::DashMatch),
        ("^=", AttrOp::Prefix),
        ("$=", AttrOp::Suffix),
        ("*=", AttrOp::Substring),
        ("=", AttrOp::Equals),
    ];
    for (tok, op) in ops {
        if let Some(pos) = body.find(tok) {
            let name = body[..pos].trim().to_string();
            let raw_val = body[pos + tok.len()..].trim();
            let value = unquote(raw_val);
            if name.is_empty() {
                return None;
            }
            return Some(AttrSel { name: name.to_ascii_lowercase(), op, value, case_insensitive });
        }
    }
    // No operator → presence test.
    let name = body.trim().to_string();
    if name.is_empty() {
        return None;
    }
    Some(AttrSel { name: name.to_ascii_lowercase(), op: AttrOp::Exists, value: String::new(), case_insensitive })
}

/// Strip optional surrounding quotes.
fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 {
        let b = s.as_bytes();
        if (b[0] == b'"' && b[s.len() - 1] == b'"') || (b[0] == b'\'' && b[s.len() - 1] == b'\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// Parse a pseudo-class by name (+ optional functional argument). Returns the pseudo and its
/// specificity contribution. `None` only for genuinely unparseable functional args.
fn parse_pseudo(name: &str, arg: Option<&str>) -> Option<(Pseudo, Spec)> {
    let class_spec = Spec { a: 0, b: 1, c: 0 };
    let p = match name {
        "first-child" => Pseudo::FirstChild,
        "last-child" => Pseudo::LastChild,
        "only-child" => Pseudo::OnlyChild,
        "first-of-type" => Pseudo::FirstOfType,
        "last-of-type" => Pseudo::LastOfType,
        "only-of-type" => Pseudo::OnlyOfType,
        "root" => Pseudo::Root,
        "empty" => Pseudo::Empty,
        "checked" => Pseudo::Checked,
        "disabled" => Pseudo::Disabled,
        "enabled" => Pseudo::Enabled,
        "required" => Pseudo::Required,
        "optional" => Pseudo::Optional,
        "link" | "any-link" => Pseudo::Link,
        "hover" => Pseudo::Hover,
        "focus" => Pseudo::Focus,
        "active" => Pseudo::Active,
        "focus-within" => Pseudo::FocusWithin,
        "focus-visible" => Pseudo::FocusVisible,
        // Best-effort never-match (parse cleanly, never match).
        "visited" | "target" | "default" | "placeholder-shown" | "read-only" | "read-write"
        | "in-range" | "out-of-range" | "valid" | "invalid" | "indeterminate" | "autofill" => {
            Pseudo::NeverMatch
        }
        "lang" => {
            let a = arg?.trim().trim_matches(|c| c == '"' || c == '\'').trim();
            if a.is_empty() { return None; }
            Pseudo::Lang(a.to_ascii_lowercase())
        }
        "nth-child" => Pseudo::NthChild(parse_nth(arg?)?),
        "nth-last-child" => Pseudo::NthLastChild(parse_nth(arg?)?),
        "nth-of-type" => Pseudo::NthOfType(parse_nth(arg?)?),
        "nth-last-of-type" => Pseudo::NthLastOfType(parse_nth(arg?)?),
        "not" => {
            let list = parse_selector_list(arg?)?;
            let s = list.iter().fold(Spec::default(), |acc, c| acc.max_with(unpack_spec(c.specificity)));
            return Some((Pseudo::Not(list), s));
        }
        "is" | "matches" => {
            let list = parse_selector_list(arg?)?;
            let s = list.iter().fold(Spec::default(), |acc, c| acc.max_with(unpack_spec(c.specificity)));
            return Some((Pseudo::Is(list), s));
        }
        "where" => {
            let list = parse_selector_list(arg?)?;
            // :where() contributes ZERO specificity.
            return Some((Pseudo::Where(list), Spec::default()));
        }
        // Unknown pseudo-class: best-effort never-match (don't drop the rule — the rest of the
        // compound may still be useful, but this element won't match it).
        _ => Pseudo::NeverMatch,
    };
    Some((p, class_spec))
}

/// Unpack a packed specificity back to components (for `:is`/`:not` "most specific arg").
fn unpack_spec(packed: u32) -> Spec {
    Spec { a: packed / 10000, b: (packed / 100) % 100, c: packed % 100 }
}

/// Parse a comma-separated selector list (the argument of `:is/:where/:not`).
fn parse_selector_list(arg: &str) -> Option<Vec<ComplexSelector>> {
    let mut out = Vec::new();
    for piece in split_selector_list(arg) {
        let p = piece.trim();
        if p.is_empty() {
            continue;
        }
        out.push(parse_complex(p)?);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Split a selector list on top-level commas (not inside [], (), or quotes).
fn split_selector_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut brk = 0i32;
    let mut par = 0i32;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            '[' => {
                brk += 1;
                cur.push(c);
            }
            ']' => {
                brk -= 1;
                cur.push(c);
            }
            '(' => {
                par += 1;
                cur.push(c);
            }
            ')' => {
                par -= 1;
                cur.push(c);
            }
            ',' if brk == 0 && par == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// Parse an `An+B` micro-syntax (`odd`, `even`, `3`, `2n`, `2n+1`, `-n+3`, `n`).
fn parse_nth(arg: &str) -> Option<NthArg> {
    let s: String = arg.trim().to_ascii_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    if s == "odd" {
        return Some(NthArg { a: 2, b: 1 });
    }
    if s == "even" {
        return Some(NthArg { a: 2, b: 0 });
    }
    if let Some(npos) = s.find('n') {
        let a_str = &s[..npos];
        let a = match a_str {
            "" | "+" => 1,
            "-" => -1,
            _ => a_str.parse::<i32>().ok()?,
        };
        let rest = &s[npos + 1..];
        let b = if rest.is_empty() {
            0
        } else {
            // rest is like "+1" / "-3".
            rest.parse::<i32>().ok()?
        };
        Some(NthArg { a, b })
    } else {
        // Plain integer B.
        Some(NthArg { a: 0, b: s.parse::<i32>().ok()? })
    }
}

/// A valid CSS identifier for our purposes: letters, digits, `-`, `_`, not starting empty.
fn is_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_name_char)
}

/// A character allowed inside a class/id/type/pseudo name.
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '\\' || !c.is_ascii()
}

// ===========================================================================================
// Matching against the tree (right-to-left, with backtracking)
// ===========================================================================================

/// Helper: borrow an element's [`ElementData`] for a node id, if it is an element.
fn el_of(doc: &dom::Document, id: dom::NodeId) -> Option<&dom::ElementData> {
    if id.0 >= doc.len() {
        return None;
    }
    match &doc.get(id).data {
        dom::NodeData::Element(e) => Some(e),
        _ => None,
    }
}

/// Element parent of `id` (skips non-element ancestors — though in practice the parent of an
/// element is the document or another element).
fn parent_of(doc: &dom::Document, id: dom::NodeId) -> Option<dom::NodeId> {
    doc.get(id).parent
}

/// Preceding *element* sibling of `id` (immediately before, skipping text/comment nodes).
fn prev_element_sibling(doc: &dom::Document, id: dom::NodeId) -> Option<dom::NodeId> {
    let parent = parent_of(doc, id)?;
    let kids = &doc.get(parent).children;
    let pos = kids.iter().position(|&c| c == id)?;
    kids[..pos].iter().rev().copied().find(|&c| el_of(doc, c).is_some())
}

/// All preceding element siblings of `id`, nearest-first.
fn prev_element_siblings(doc: &dom::Document, id: dom::NodeId) -> Vec<dom::NodeId> {
    let Some(parent) = parent_of(doc, id) else { return Vec::new() };
    let kids = &doc.get(parent).children;
    let Some(pos) = kids.iter().position(|&c| c == id) else { return Vec::new() };
    kids[..pos].iter().rev().copied().filter(|&c| el_of(doc, c).is_some()).collect()
}

/// Match a full complex selector against node `id` (right-to-left with backtracking).
fn complex_matches(doc: &dom::Document, id: dom::NodeId, sel: &ComplexSelector) -> bool {
    // Subject (parts[0]) must match `id`; then recurse leftward.
    if el_of(doc, id).is_none() {
        return false;
    }
    if !compound_matches(doc, id, &sel.parts[0].1) {
        return false;
    }
    match_from(doc, id, &sel.parts, 1)
}

/// Match the remaining parts `sel[idx..]` against the tree, given that `sel[idx-1]` matched at
/// `node`. Each part carries the combinator relating it to the part on its right.
fn match_from(doc: &dom::Document, node: dom::NodeId, parts: &[(Combinator, Compound)], idx: usize) -> bool {
    if idx >= parts.len() {
        return true;
    }
    let (comb, compound) = &parts[idx];
    match comb {
        Combinator::Subject => true, // shouldn't happen past index 0
        Combinator::Child => {
            if let Some(p) = parent_of(doc, node) {
                if el_of(doc, p).is_some() && compound_matches(doc, p, compound) {
                    return match_from(doc, p, parts, idx + 1);
                }
            }
            false
        }
        Combinator::Descendant => {
            // Try each ancestor; backtrack.
            let mut cur = parent_of(doc, node);
            while let Some(a) = cur {
                if el_of(doc, a).is_some()
                    && compound_matches(doc, a, compound)
                    && match_from(doc, a, parts, idx + 1)
                {
                    return true;
                }
                cur = parent_of(doc, a);
            }
            false
        }
        Combinator::NextSibling => {
            if let Some(s) = prev_element_sibling(doc, node) {
                if compound_matches(doc, s, compound) {
                    return match_from(doc, s, parts, idx + 1);
                }
            }
            false
        }
        Combinator::SubsequentSibling => {
            for s in prev_element_siblings(doc, node) {
                if compound_matches(doc, s, compound) && match_from(doc, s, parts, idx + 1) {
                    return true;
                }
            }
            false
        }
    }
}

/// Does node `id` (which must be an element) match a single compound selector?
fn compound_matches(doc: &dom::Document, id: dom::NodeId, c: &Compound) -> bool {
    let Some(el) = el_of(doc, id) else { return false };
    if let Some(t) = &c.type_part {
        if !el.tag.eq_ignore_ascii_case(t) {
            return false;
        }
    }
    // Namespace constraint on the (possibly implied universal) type selector. Only enforced when an
    // `@namespace` rule is in scope; otherwise every constraint matches (so non-namespaced pages are
    // unaffected). `el.namespace == None` means the HTML namespace.
    if !namespace_matches(&c.type_ns, el.namespace.as_deref()) {
        return false;
    }
    for want in &c.ids {
        match el.id() {
            Some(eid) if eid == want => {}
            _ => return false,
        }
    }
    for class in &c.classes {
        if !el.classes().any(|cl| cl == class) {
            return false;
        }
    }
    for attr in &c.attrs {
        if !attr_matches(el, attr) {
            return false;
        }
    }
    for p in &c.pseudos {
        if !pseudo_matches(doc, id, el, p) {
            return false;
        }
    }
    true
}

/// The XHTML namespace URI. An element with `namespace == None` (a normal HTML element) is treated
/// as being in this namespace for `@namespace` matching.
const XHTML_NS: &str = "http://www.w3.org/1999/xhtml";

/// Does a selector's namespace constraint match an element's namespace? `el_ns` is the element's
/// namespace URI (`None` = HTML, treated as the XHTML namespace). Only enforced when an
/// `@namespace` environment is in scope; with no `@namespace` rules every constraint passes, so
/// ordinary (non-namespaced) pages match exactly as they did before this feature.
fn namespace_matches(constraint: &NsConstraint, el_ns: Option<&str>) -> bool {
    // The common case: no namespace component on the selector and no `@namespace` rule in scope.
    // Resolve without touching the bindings (and never constrain) so non-namespaced pages are fast
    // and unaffected.
    if *constraint == NsConstraint::Unspecified {
        return NAMESPACE_BINDINGS.with(|c| {
            let env = c.borrow();
            match &env.default_ns {
                // With a declared default namespace, an unspecified type is constrained to it.
                Some(def) => el_ns.unwrap_or(XHTML_NS) == def,
                None => true, // no default → matches any namespace
            }
        });
    }
    NAMESPACE_BINDINGS.with(|c| {
        let env = c.borrow();
        // The element's effective namespace URI (HTML elements default to XHTML).
        let el_uri = el_ns.unwrap_or(XHTML_NS);
        match constraint {
            NsConstraint::Any => true,
            // `|tag` = the null namespace: matches only elements explicitly in no namespace.
            NsConstraint::None => el_uri.is_empty(),
            NsConstraint::Unspecified => match &env.default_ns {
                // With a default namespace, an unspecified type is constrained to it.
                Some(def) => el_uri == def,
                // No default namespace → matches any namespace.
                None => true,
            },
            NsConstraint::Prefixed(prefix) => match env.lookup(prefix) {
                Some(uri) => el_uri == uri,
                // Unknown prefix → matches nothing (invalid selector, treated as non-matching).
                None => false,
            },
        }
    })
}

/// Match one attribute selector against an element. Attribute *names* are matched
/// case-insensitively (HTML); values per the operator and the `i` flag.
fn attr_matches(el: &dom::ElementData, a: &AttrSel) -> bool {
    // Find the attribute case-insensitively by name.
    let actual = el
        .attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(&a.name))
        .map(|(_, v)| v.as_str());
    let Some(val) = actual else {
        return false;
    };
    if a.op == AttrOp::Exists {
        return true;
    }
    let (hay, needle) = if a.case_insensitive {
        (val.to_ascii_lowercase(), a.value.to_ascii_lowercase())
    } else {
        (val.to_string(), a.value.clone())
    };
    match a.op {
        AttrOp::Exists => true,
        AttrOp::Equals => hay == needle,
        AttrOp::Includes => !needle.is_empty() && hay.split_whitespace().any(|w| w == needle),
        AttrOp::DashMatch => hay == needle || hay.starts_with(&format!("{needle}-")),
        AttrOp::Prefix => !needle.is_empty() && hay.starts_with(&needle),
        AttrOp::Suffix => !needle.is_empty() && hay.ends_with(&needle),
        AttrOp::Substring => !needle.is_empty() && hay.contains(&needle),
    }
}

/// Match a pseudo-class against an element node.
fn pseudo_matches(doc: &dom::Document, id: dom::NodeId, el: &dom::ElementData, p: &Pseudo) -> bool {
    match p {
        Pseudo::Root => el.tag.eq_ignore_ascii_case("html"),
        Pseudo::FirstChild => element_index(doc, id).map(|(i, _)| i == 0).unwrap_or(false),
        Pseudo::LastChild => element_index(doc, id).map(|(i, t)| i + 1 == t).unwrap_or(false),
        Pseudo::OnlyChild => element_index(doc, id).map(|(_, t)| t == 1).unwrap_or(false),
        Pseudo::NthChild(n) => element_index(doc, id).map(|(i, _)| n.matches(i as i32 + 1)).unwrap_or(false),
        Pseudo::NthLastChild(n) => element_index(doc, id).map(|(i, t)| n.matches((t - i) as i32)).unwrap_or(false),
        Pseudo::FirstOfType => type_index(doc, id, &el.tag).map(|(i, _)| i == 0).unwrap_or(false),
        Pseudo::LastOfType => type_index(doc, id, &el.tag).map(|(i, t)| i + 1 == t).unwrap_or(false),
        Pseudo::OnlyOfType => type_index(doc, id, &el.tag).map(|(_, t)| t == 1).unwrap_or(false),
        Pseudo::NthOfType(n) => type_index(doc, id, &el.tag).map(|(i, _)| n.matches(i as i32 + 1)).unwrap_or(false),
        Pseudo::NthLastOfType(n) => type_index(doc, id, &el.tag).map(|(i, t)| n.matches((t - i) as i32)).unwrap_or(false),
        Pseudo::Empty => is_empty_element(doc, id),
        Pseudo::Checked => {
            (el.tag.eq_ignore_ascii_case("input") || el.tag.eq_ignore_ascii_case("option"))
                && el.attrs.keys().any(|k| k.eq_ignore_ascii_case("checked") || k.eq_ignore_ascii_case("selected"))
        }
        Pseudo::Disabled => is_form_control(&el.tag) && has_attr(el, "disabled"),
        Pseudo::Enabled => is_form_control(&el.tag) && !has_attr(el, "disabled"),
        Pseudo::Required => is_form_control(&el.tag) && has_attr(el, "required"),
        Pseudo::Optional => is_form_control(&el.tag) && !has_attr(el, "required"),
        Pseudo::Link => el.tag.eq_ignore_ascii_case("a") && has_attr(el, "href"),
        Pseudo::Hover => {
            let h = interaction_hovered();
            h == Some(id.0) || h.map(|hn| is_ancestor(doc, id, dom::NodeId(hn))).unwrap_or(false)
        }
        // `:active` ≈ `:hover` (no separate pressed-state tracking in the engine).
        Pseudo::Active => {
            let h = interaction_hovered();
            h == Some(id.0) || h.map(|hn| is_ancestor(doc, id, dom::NodeId(hn))).unwrap_or(false)
        }
        Pseudo::Focus | Pseudo::FocusVisible => interaction_focused() == Some(id.0),
        Pseudo::FocusWithin => {
            let f = interaction_focused();
            f == Some(id.0) || f.map(|fn_| is_ancestor(doc, id, dom::NodeId(fn_))).unwrap_or(false)
        }
        Pseudo::Lang(want) => element_lang(doc, id)
            .map(|l| {
                let l = l.to_ascii_lowercase();
                l == *want || l.starts_with(&format!("{want}-"))
            })
            .unwrap_or(false),
        Pseudo::Not(list) => !list.iter().any(|s| complex_matches(doc, id, s)),
        Pseudo::Is(list) | Pseudo::Where(list) => list.iter().any(|s| complex_matches(doc, id, s)),
        Pseudo::NeverMatch => false,
    }
}

fn has_attr(el: &dom::ElementData, name: &str) -> bool {
    el.attrs.keys().any(|k| k.eq_ignore_ascii_case(name))
}

/// The content language of element `id` per HTML: the `lang` (or `xml:lang`) attribute of the
/// nearest inclusive ancestor that has one. Returns `None` if no ancestor sets a language.
fn element_lang(doc: &dom::Document, id: dom::NodeId) -> Option<String> {
    let mut cur = Some(id);
    while let Some(n) = cur {
        if let Some(el) = el_of(doc, n) {
            if let Some((_, v)) = el
                .attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("lang") || k.eq_ignore_ascii_case("xml:lang"))
            {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
        cur = parent_of(doc, n);
    }
    None
}

fn is_form_control(tag: &str) -> bool {
    matches!(
        tag.to_ascii_lowercase().as_str(),
        "input" | "button" | "select" | "textarea" | "option" | "optgroup" | "fieldset"
    )
}

/// Is `ancestor` an ancestor of `descendant` (strictly above it)?
fn is_ancestor(doc: &dom::Document, ancestor: dom::NodeId, descendant: dom::NodeId) -> bool {
    if descendant.0 >= doc.len() {
        return false;
    }
    let mut cur = doc.get(descendant).parent;
    while let Some(p) = cur {
        if p == ancestor {
            return true;
        }
        cur = doc.get(p).parent;
    }
    false
}

/// (index-among-element-siblings, total-element-siblings) for `id`.
fn element_index(doc: &dom::Document, id: dom::NodeId) -> Option<(usize, usize)> {
    let parent = parent_of(doc, id)?;
    let kids = &doc.get(parent).children;
    let elems: Vec<dom::NodeId> = kids.iter().copied().filter(|&c| el_of(doc, c).is_some()).collect();
    let pos = elems.iter().position(|&c| c == id)?;
    Some((pos, elems.len()))
}

/// (index-among-same-type-siblings, total-same-type-siblings) for `id`.
fn type_index(doc: &dom::Document, id: dom::NodeId, tag: &str) -> Option<(usize, usize)> {
    let parent = parent_of(doc, id)?;
    let kids = &doc.get(parent).children;
    let same: Vec<dom::NodeId> = kids
        .iter()
        .copied()
        .filter(|&c| el_of(doc, c).map(|e| e.tag.eq_ignore_ascii_case(tag)).unwrap_or(false))
        .collect();
    let pos = same.iter().position(|&c| c == id)?;
    Some((pos, same.len()))
}

/// `:empty` — no element children and no non-whitespace text.
fn is_empty_element(doc: &dom::Document, id: dom::NodeId) -> bool {
    for &c in &doc.get(id).children {
        if c.0 >= doc.len() {
            continue;
        }
        match &doc.get(c).data {
            dom::NodeData::Element(_) => return false,
            dom::NodeData::Text(t) if !t.trim().is_empty() => return false,
            _ => {}
        }
    }
    true
}

/// The built-in user-agent stylesheet: sane defaults on a white page canvas.
fn user_agent_stylesheet() -> css::Stylesheet {
    // html/body default text color is themed by the root's used `color-scheme` (resolved by the
    // cascade pre-pass before this runs): black on a light page, light grey (`#e8e8e8`) on a dark
    // one. Form-control text/background (`input`/`button`/…) is intentionally left light — control
    // & scrollbar theming is out of scope.
    let (tr, tg, tb) = ua_default_text_color();
    let text = format!("#{tr:02x}{tg:02x}{tb:02x}");
    let sheet =
        // html/body keep explicit UA color rules (rather than dropping them) so body's color doesn't
        // inherit a `:root` author color the way a real `:root` selector would; author rules still
        // override.
        "html { color: {TEXT}; font-size: 16px }
         body { color: {TEXT}; font-size: 16px }
         h1 { font-size: 32px; font-weight: bold; display: block; margin: 0.67em 0 }
         h2 { font-size: 26px; font-weight: bold; display: block; margin: 0.83em 0 }
         h3 { font-size: 20px; font-weight: bold; display: block; margin: 1em 0 }
         h4 { font-size: 17px; font-weight: bold; display: block; margin: 1.33em 0 }
         h5 { font-size: 15px; font-weight: bold; display: block; margin: 1.67em 0 }
         h6 { font-size: 13px; font-weight: bold; display: block; margin: 2.33em 0 }
         p { display: block; margin: 1em 0 }
         div { display: block }
         section { display: block }
         article { display: block }
         header { display: block }
         footer { display: block }
         nav { display: block }
         main { display: block }
         aside { display: block }
         ul { display: block; margin: 1em 0; padding-left: 40px; list-style-type: disc }
         ol { display: block; margin: 1em 0; padding-left: 40px; list-style-type: decimal }
         li { display: block }
         blockquote { display: block; margin: 1em 40px }
         pre { display: block; margin: 1em 0; white-space: pre }
         table { display: table }
         tr { display: table-row }
         td, th { display: table-cell; padding: 1px }
         th { font-weight: bold; text-align: center }
         thead { display: table-header-group }
         tbody { display: table-row-group }
         tfoot { display: table-footer-group }
         colgroup { display: table-column-group }
         col { display: table-column }
         details { display: block }
         summary { display: block }
         figure { display: block; margin: 1em 40px }
         figcaption { display: block }
         fieldset { display: block }
         legend { display: block }
         form { display: block }
         dl { display: block; margin: 1em 0 }
         dt { display: block }
         dd { display: block; margin-left: 40px }
         address { display: block }
         dialog { display: none; margin: auto; padding: 1em; border: 2px solid #767676; background-color: #ffffff; color: #000000 }
         dialog[open] { display: block }
         hr { display: block; margin: 0.5em 0; height: 1px; background-color: #888888; border-top: 1px solid #888888 }
         caption { display: table-caption }
         details:not([open]) > :not(summary) { display: none }
         summary::before { content: \"\\25B8 \" }
         details[open] > summary::before { content: \"\\25BE \" }
         b { font-weight: bold }
         strong { font-weight: bold }
         i { font-style: italic }
         em { font-style: italic }
         a { text-decoration: underline; color: #0000ee }
         u, ins { text-decoration: underline }
         s, del, strike { text-decoration: line-through }
         abbr[title] { text-decoration: underline }
         mark { background-color: #ffff00; color: #000 }
         cite, var, dfn, address { font-style: italic }
         small { font-size: smaller }
         sub, sup { font-size: smaller }
         sub { vertical-align: sub }
         sup { vertical-align: super }
         q::before { content: \"\\201C\" }
         q::after { content: \"\\201D\" }
         input, textarea, select, button { display: inline-block; border: 1px solid #767676; color: #000000; background-color: #ffffff; padding: 1px 2px }
         input[type=submit], input[type=reset], input[type=button], button { background-color: #efefef; padding: 2px 8px }
         input[type=file] { background-color: #efefef; padding: 1px 2px }
         input[type=checkbox], input[type=radio], input[type=range], input[type=color], progress, meter { border: 0; padding: 0; background-color: transparent }
         label { display: inline-block }";
    css::parse(&sheet.replace("{TEXT}", &text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom::NodeData;

    /// Serializes tests that read or mutate the process-global OS-appearance / root-color-scheme
    /// flags (`set_color_scheme_dark` / `root_used_scheme_dark`), which `cargo test` would otherwise
    /// run in parallel and race on. Poisoning is irrelevant (we only need exclusion), so callers
    /// ignore a poisoned guard.
    static SCHEME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn scheme_guard() -> std::sync::MutexGuard<'static, ()> {
        SCHEME_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn elem(doc: &dom::Document, tag_and_pred: impl Fn(&dom::ElementData) -> bool) -> dom::NodeId {
        // Find first element matching predicate (depth-first).
        fn walk(
            doc: &dom::Document,
            id: dom::NodeId,
            pred: &dyn Fn(&dom::ElementData) -> bool,
        ) -> Option<dom::NodeId> {
            if let NodeData::Element(e) = &doc.get(id).data {
                if pred(e) {
                    return Some(id);
                }
            }
            for &c in &doc.get(id).children {
                if let Some(found) = walk(doc, c, pred) {
                    return Some(found);
                }
            }
            None
        }
        walk(doc, doc.root(), &tag_and_pred).expect("element not found")
    }

    // ------------------------------------------------------------------------------------------
    // Gradient / box-shadow / transform value parsing
    // ------------------------------------------------------------------------------------------

    fn grad(val: &str) -> Gradient {
        parse_gradient(val, (0, 0, 0), (0, 0, 0)).expect("expected a gradient")
    }

    #[test]
    fn linear_gradient_angle_two_stops() {
        match grad("linear-gradient(90deg, red, blue)") {
            Gradient::Linear { angle_deg, stops } => {
                assert_eq!(angle_deg, 90.0);
                assert_eq!(stops.len(), 2);
                assert!((stops[0].pos - 0.0).abs() < 1e-6);
                assert!((stops[1].pos - 1.0).abs() < 1e-6);
                assert_eq!(stops[0].color, Rgba { r: 255, g: 0, b: 0, a: 255 });
                assert_eq!(stops[1].color, Rgba { r: 0, g: 0, b: 255, a: 255 });
            }
            _ => panic!("expected linear"),
        }
    }

    #[test]
    fn linear_gradient_to_right_with_percent_stops() {
        match grad("linear-gradient(to right, #fff 0%, #000 100%)") {
            Gradient::Linear { angle_deg, stops } => {
                assert_eq!(angle_deg, 90.0); // to right == 90deg
                assert_eq!(stops.len(), 2);
                assert_eq!(stops[0].color, Rgba { r: 255, g: 255, b: 255, a: 255 });
                assert!((stops[0].pos - 0.0).abs() < 1e-6);
                assert!((stops[1].pos - 1.0).abs() < 1e-6);
            }
            _ => panic!("expected linear"),
        }
    }

    #[test]
    fn linear_gradient_distributes_three_unpositioned_stops() {
        match grad("linear-gradient(red, green, blue)") {
            Gradient::Linear { stops, .. } => {
                assert_eq!(stops.len(), 3);
                assert!((stops[0].pos - 0.0).abs() < 1e-6);
                assert!((stops[1].pos - 0.5).abs() < 1e-6);
                assert!((stops[2].pos - 1.0).abs() < 1e-6);
            }
            _ => panic!("expected linear"),
        }
    }

    #[test]
    fn radial_gradient_parses() {
        match grad("radial-gradient(red, blue)") {
            Gradient::Radial { stops } => {
                assert_eq!(stops.len(), 2);
                assert_eq!(stops[0].color, Rgba { r: 255, g: 0, b: 0, a: 255 });
            }
            _ => panic!("expected radial"),
        }
    }

    #[test]
    fn repeating_linear_treated_as_linear() {
        assert!(matches!(grad("repeating-linear-gradient(0deg, red, blue)"), Gradient::Linear { .. }));
    }

    #[test]
    fn box_shadow_single_with_rgba() {
        let s = parse_box_shadows("2px 4px 8px rgba(0,0,0,.5)", (0, 0, 0), (0, 0, 0));
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].dx, 2.0);
        assert_eq!(s[0].dy, 4.0);
        assert_eq!(s[0].blur, 8.0);
        assert_eq!(s[0].spread, 0.0);
        assert!(!s[0].inset);
        assert_eq!(s[0].color.a, 128);
    }

    #[test]
    fn box_shadow_two_layers() {
        let s = parse_box_shadows("2px 2px 4px black, 0 0 10px red", (0, 0, 0), (0, 0, 0));
        assert_eq!(s.len(), 2);
        assert_eq!(s[1].blur, 10.0);
        assert_eq!(s[1].color, Rgba { r: 255, g: 0, b: 0, a: 255 });
    }

    #[test]
    fn box_shadow_inset_with_spread() {
        let s = parse_box_shadows("inset 1px 2px 3px 4px #000", (0, 0, 0), (0, 0, 0));
        assert_eq!(s.len(), 1);
        assert!(s[0].inset);
        assert_eq!(s[0].spread, 4.0);
    }

    #[test]
    fn transform_translate_scale_composes() {
        let m = parse_transform("translate(10px, 20px) scale(2)").expect("matrix");
        // Composed: first translate then scale (translate outermost). Apply to origin (0,0):
        // result = T * S * (0,0) = (10, 20). Apply to (1,1): T*S = scale then translate.
        // x' = a*x + c*y + e = 2*1 + 0 + 10 = 12; y' = b*x + d*y + f = 0 + 2*1 + 20 = 22.
        assert_eq!(m[0], 2.0); // a (scale x)
        assert_eq!(m[3], 2.0); // d (scale y)
        assert_eq!(m[4], 10.0); // e
        assert_eq!(m[5], 20.0); // f
    }

    #[test]
    fn transform_rotate_90_matrix() {
        let m = parse_transform("rotate(90deg)").expect("matrix");
        // rotate(90deg): cos=0, sin=1 → [0, 1, -1, 0, 0, 0].
        assert!((m[0] - 0.0).abs() < 1e-5, "a={}", m[0]);
        assert!((m[1] - 1.0).abs() < 1e-5, "b={}", m[1]);
        assert!((m[2] - (-1.0)).abs() < 1e-5, "c={}", m[2]);
        assert!((m[3] - 0.0).abs() < 1e-5, "d={}", m[3]);
    }

    #[test]
    fn transform_matrix_passthrough() {
        let m = parse_transform("matrix(1,2,3,4,5,6)").expect("matrix");
        assert_eq!(m, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn transform_origin_top_left() {
        assert_eq!(parse_transform_origin("top left"), (0.0, 0.0));
        assert_eq!(parse_transform_origin("left top"), (0.0, 0.0));
        assert_eq!(parse_transform_origin("bottom right"), (1.0, 1.0));
        assert_eq!(parse_transform_origin("center"), (0.5, 0.5));
        assert_eq!(parse_transform_origin("50% 50%"), (0.5, 0.5));
    }

    #[test]
    fn gradient_applied_via_cascade_background() {
        let doc = html::parse(
            r#"<html><body><div style="background: linear-gradient(to right, red, blue)">x</div></body></html>"#,
        );
        let map = cascade(&doc, &[]);
        let div = elem(&doc, |e| e.tag == "div");
        assert!(map[&div].background_gradient.is_some());
        // Solid background-color must remain unset when a gradient is used.
        assert!(map[&div].background_color.is_none());
    }

    #[test]
    fn box_shadow_and_transform_via_cascade() {
        let doc = html::parse(
            r#"<html><body><div style="box-shadow: 2px 4px 8px black; transform: translate(10px,20px) scale(2); transform-origin: top left">x</div></body></html>"#,
        );
        let map = cascade(&doc, &[]);
        let div = elem(&doc, |e| e.tag == "div");
        assert_eq!(map[&div].box_shadows.len(), 1);
        assert_eq!(map[&div].transform, Some([2.0, 0.0, 0.0, 2.0, 10.0, 20.0]));
        assert_eq!(map[&div].transform_origin, (0.0, 0.0));
    }

    #[test]
    fn cascade_runs_on_empty_inputs() {
        let doc = dom::Document::new();
        let map = cascade(&doc, &[]);
        assert!(map.is_empty());
    }

    #[test]
    fn namespace_type_selector_matches_only_svg() {
        // With a default (xhtml) @namespace and a `svg` prefix, `svg|*` matches the SVG element but
        // not the HTML one; an unprefixed selector is constrained to the default (xhtml) namespace.
        let doc = html::parse("<html><body><div class=x></div><svg class=x></svg></body></html>");
        let sheet = css::parse(
            "@namespace url(http://www.w3.org/1999/xhtml); \
             @namespace svg url(http://www.w3.org/2000/svg); \
             svg|*.x { color: rgb(1, 2, 3); } \
             .x { background-color: rgb(4, 5, 6); }",
        );
        let map = cascade(&doc, std::slice::from_ref(&sheet));
        let svg = elem(&doc, |e| e.tag == "svg");
        let div = elem(&doc, |e| e.tag == "div");
        // svg|*.x matched the SVG element (color set), not the HTML div.
        assert_eq!(map[&svg].color, (1, 2, 3));
        assert_ne!(map[&div].color, (1, 2, 3));
        // The unprefixed `.x` is constrained to the default xhtml namespace -> matches the HTML div,
        // not the SVG element.
        assert_eq!(map[&div].background_color, Some((4, 5, 6)));
        assert_eq!(map[&svg].background_color, None);
    }

    #[test]
    fn registered_property_seeds_initial_value() {
        let doc = html::parse("<html><body><div id=t></div></body></html>");
        let sheet = css::parse(
            "@property --reg { syntax: \"<length>\"; inherits: false; initial-value: 7px; }",
        );
        let map = cascade(&doc, std::slice::from_ref(&sheet));
        let div = elem(&doc, |e| e.attrs.get("id").map(|s| s == "t").unwrap_or(false));
        // The registered property's initial value is present in the element's custom-prop env even
        // though it was never explicitly set.
        assert_eq!(map[&div].custom_props.get("--reg").map(String::as_str), Some("7px"));
    }

    #[test]
    fn ua_defaults_make_h1_big_and_bold() {
        let doc = html::parse("<html><body><h1>Hi</h1></body></html>");
        let map = cascade(&doc, &[]);
        let h1 = elem(&doc, |e| e.tag == "h1");
        let s = &map[&h1];
        assert_eq!(s.font_size, 32.0);
        assert!(s.bold);
        assert!(s.display_block);
    }

    #[test]
    fn ua_default_p_margin_is_one_em() {
        // The UA sheet gives <p> `margin: 1em 0`; with the default 16px font that's 16px top/bottom.
        let doc = html::parse("<html><body><p>x</p></body></html>");
        let map = cascade(&doc, &[]);
        let p = elem(&doc, |e| e.tag == "p");
        let s = &map[&p];
        assert_eq!(s.margin.top, 16.0, "p margin-top should be 1em = 16px");
        assert_eq!(s.margin.bottom, 16.0);
        assert_eq!(s.margin.left, 0.0);
        // getComputedStyle string form.
        assert_eq!(s.get_property("margin-top"), "16px");
    }

    #[test]
    fn ua_em_margin_scales_with_heading_font_size() {
        // h1 has font-size 32px and `margin: 0.67em 0` → 0.67 * 32 ≈ 21.44px (resolved against the
        // element's OWN font size, not the 16px default).
        let doc = html::parse("<html><body><h1>Hi</h1></body></html>");
        let map = cascade(&doc, &[]);
        let h1 = elem(&doc, |e| e.tag == "h1");
        let mt = map[&h1].margin.top;
        assert!((mt - 0.67 * 32.0).abs() < 0.01, "h1 margin-top {mt} should be 0.67em of 32px");
    }

    #[test]
    fn ua_ul_padding_and_list_style_and_pre_white_space() {
        let doc = html::parse(
            "<html><body><ul><li>a</li></ul><ol><li>b</li></ol><pre>code</pre></body></html>",
        );
        let map = cascade(&doc, &[]);
        let ul = elem(&doc, |e| e.tag == "ul");
        assert_eq!(map[&ul].padding.left, 40.0, "ul padding-left 40px");
        assert_eq!(map[&ul].list_style_type, ListStyleType::Disc);
        let ol = elem(&doc, |e| e.tag == "ol");
        assert_eq!(map[&ol].list_style_type, ListStyleType::Decimal);
        let pre = elem(&doc, |e| e.tag == "pre");
        assert_eq!(map[&pre].white_space, WhiteSpace::Pre);
        assert_eq!(map[&pre].get_property("white-space"), "pre");
    }

    #[test]
    fn ua_hr_has_height_and_background() {
        let doc = html::parse("<html><body><hr></body></html>");
        let map = cascade(&doc, &[]);
        let hr = elem(&doc, |e| e.tag == "hr");
        let s = &map[&hr];
        assert_eq!(s.height, Some(1.0), "hr should have a 1px height so it paints");
        assert!(s.background_color.is_some(), "hr should have a visible background fill");
    }

    #[test]
    fn lang_pseudo_class_matches_inherited_language() {
        // `:lang(zh)` and `:lang(zh-CN)` both match an element whose inherited lang is `zh-CN`;
        // `:lang(tr)` does not.
        let sheet = css::parse(
            ":lang(zh) { color: #010101 }
             :lang(zh-CN) { background-color: #020202 }
             :lang(tr) { font-size: 99px }",
        );
        let doc = html::parse(
            r#"<html><body><div lang="zh-CN"><span>x</span></div></body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let span = elem(&doc, |e| e.tag == "span");
        // Inherited lang from the ancestor div: :lang(zh) and :lang(zh-CN) match (color/bg set),
        // :lang(tr) does not (font-size stays the inherited default, not 99px).
        assert_eq!(map[&span].color, (1, 1, 1), "span :lang(zh) should set color");
        assert_eq!(map[&span].background_color, Some((2, 2, 2)), "span :lang(zh-CN) should set bg");
        assert!((map[&span].font_size - 99.0).abs() > 0.5, ":lang(tr) must not match");
    }

    #[test]
    fn font_family_serializes_with_canonical_quoting() {
        // Generic families/CSS-wide keywords stay quoted; valid ident sequences unquote.
        assert_eq!(serialize_font_family("'Times New Roman'"), "Times New Roman");
        assert_eq!(serialize_font_family("\"serif\""), "\"serif\"");
        assert_eq!(serialize_font_family("'34J'"), "\"34J\"");
        assert_eq!(serialize_font_family("'A  B'"), "\"A  B\"");
        assert_eq!(serialize_font_family("Veronica"), "Veronica");
        assert_eq!(
            serialize_font_family("Twisty Tie, '34J', \"serif\", Veronica, sans-serif"),
            "Twisty Tie, \"34J\", \"serif\", Veronica, sans-serif"
        );
    }

    #[test]
    fn white_space_pre_parses() {
        let doc = html::parse(r#"<html><body><span style="white-space: pre">x</span></body></html>"#);
        let map = cascade(&doc, &[]);
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&span].white_space, WhiteSpace::Pre);
    }

    #[test]
    fn id_beats_class_beats_type() {
        let sheet = css::parse(
            "p { color: red } .c { color: green } #x { color: blue }",
        );
        let doc = html::parse(r#"<html><body><p id="x" class="c">t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        // id selector (#x) wins → blue.
        assert_eq!(map[&p].color, (0, 0, 255));
    }

    #[test]
    fn class_beats_type() {
        let sheet = css::parse("p { color: red } .c { color: green }");
        let doc = html::parse(r#"<html><body><p class="c">t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (0, 128, 0));
    }

    #[test]
    fn inline_beats_sheet() {
        let sheet = css::parse("#x { color: blue }");
        let doc = html::parse(r#"<html><body><p id="x" style="color: red">t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (255, 0, 0));
    }

    #[test]
    fn color_and_font_size_inherit_to_children() {
        let sheet = css::parse("#wrap { color: #ff0000; font-size: 24px }");
        let doc = html::parse(
            r#"<html><body><div id="wrap"><span>child</span></div></body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&span].color, (255, 0, 0));
        assert_eq!(map[&span].font_size, 24.0);
    }

    #[test]
    fn display_none_propagates_to_subtree() {
        let sheet = css::parse("#h { display: none }");
        let doc = html::parse(
            r#"<html><body><div id="h"><p>hidden</p></div><p>shown</p></body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let hidden_div = elem(&doc, |e| e.id() == Some("h"));
        assert!(map[&hidden_div].display_none);
        // The nested <p> inherits hidden-ness.
        let inner = elem(&doc, |e| {
            e.tag == "p"
                // the hidden one is the first <p>
        });
        // First matching p in doc order is the hidden one.
        assert!(map[&inner].display_none);
    }

    #[test]
    fn compound_selector_matches() {
        let sheet = css::parse("p.note { color: orange }");
        let doc = html::parse(
            r#"<html><body><p class="note">a</p><p>b</p></body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let note = elem(&doc, |e| e.tag == "p" && e.classes().any(|c| c == "note"));
        assert_eq!(map[&note].color, (255, 165, 0));
    }

    #[test]
    fn named_and_hex_colors_parse() {
        assert_eq!(parse_color("#f00"), Some((255, 0, 0)));
        assert_eq!(parse_color("#00ff00"), Some((0, 255, 0)));
        assert_eq!(parse_color("blue"), Some((0, 0, 255)));
        assert_eq!(parse_color("nope"), None);
    }

    #[test]
    fn font_sizes_parse() {
        assert_eq!(parse_font_size("20px", 16.0), Some(20.0));
        assert_eq!(parse_font_size("12pt", 16.0), Some(16.0));
        assert_eq!(parse_font_size("2em", 16.0), Some(32.0));
    }

    #[test]
    fn margin_shorthand_one_value() {
        assert_eq!(parse_edges_shorthand("10px", 16.0), Some(Edges::all(10.0)));
    }

    #[test]
    fn margin_shorthand_two_values() {
        // vertical horizontal
        assert_eq!(
            parse_edges_shorthand("10px 20px", 16.0),
            Some(Edges { top: 10.0, bottom: 10.0, right: 20.0, left: 20.0 })
        );
    }

    #[test]
    fn margin_shorthand_four_values() {
        // top right bottom left
        assert_eq!(
            parse_edges_shorthand("1px 2px 3px 4px", 16.0),
            Some(Edges { top: 1.0, right: 2.0, bottom: 3.0, left: 4.0 })
        );
    }

    #[test]
    fn margin_applied_via_cascade() {
        let sheet = css::parse("p { margin: 5px 10px }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].margin, Edges { top: 5.0, bottom: 5.0, right: 10.0, left: 10.0 });
    }

    #[test]
    fn per_side_override_and_specificity() {
        // The longhand override and a higher-specificity rule both apply on top of shorthand.
        let sheet = css::parse(
            "p { margin: 4px; margin-left: 12px } .x { margin-top: 20px }",
        );
        let doc = html::parse(r#"<html><body><p class="x">t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        let m = map[&p].margin;
        assert_eq!(m.left, 12.0); // longhand overrode shorthand
        assert_eq!(m.top, 20.0); // higher specificity .x rule wins
        assert_eq!(m.right, 4.0); // untouched shorthand value
        assert_eq!(m.bottom, 4.0);
    }

    #[test]
    fn padding_shorthand_three_values() {
        let sheet = css::parse("div { padding: 1px 2px 3px }");
        let doc = html::parse(r#"<html><body><div>t</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        assert_eq!(
            map[&d].padding,
            Edges { top: 1.0, right: 2.0, left: 2.0, bottom: 3.0 }
        );
    }

    #[test]
    fn border_shorthand_width_and_color() {
        let sheet = css::parse("div { border: 2px solid #ff0000 }");
        let doc = html::parse(r#"<html><body><div>t</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        assert_eq!(map[&d].border, Edges::all(2.0));
        assert_eq!(map[&d].border_color, (255, 0, 0));
    }

    #[test]
    fn border_none_is_zero() {
        let sheet = css::parse("div { border: none }");
        let doc = html::parse(r#"<html><body><div>t</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        assert_eq!(map[&d].border, Edges::all(0.0));
    }

    #[test]
    fn ua_table_display_values() {
        // The UA stylesheet maps table tags to their table-* display values, and styles <th>.
        let doc = html::parse(
            r#"<html><body><table>
                <caption>Cap</caption>
                <thead><tr><th>H</th></tr></thead>
                <tbody><tr><td>D</td></tr></tbody>
                <tfoot><tr><td>F</td></tr></tfoot>
                <colgroup><col></colgroup>
            </table></body></html>"#,
        );
        let map = cascade(&doc, &[]);
        let d = |tag: &str| map[&elem(&doc, |e| e.tag == tag)].display;
        assert_eq!(d("table"), Display::Table);
        assert_eq!(d("tr"), Display::TableRow);
        assert_eq!(d("td"), Display::TableCell);
        assert_eq!(d("th"), Display::TableCell);
        assert_eq!(d("thead"), Display::TableHeaderGroup);
        assert_eq!(d("tbody"), Display::TableRowGroup);
        assert_eq!(d("tfoot"), Display::TableFooterGroup);
        assert_eq!(d("caption"), Display::TableCaption);
        assert_eq!(d("colgroup"), Display::TableColumnGroup);
        assert_eq!(d("col"), Display::TableColumn);
        // <th> defaults: bold + centered + 1px padding (the cells get a little padding).
        let th = map[&elem(&doc, |e| e.tag == "th")].clone();
        assert!(th.bold, "th should be bold");
        assert_eq!(th.text_align, TextAlign::Center);
        assert_eq!(th.padding, Edges::all(1.0));
        // getComputedStyle reports the table display string.
        assert_eq!(map[&elem(&doc, |e| e.tag == "table")].get_property("display"), "table");
    }

    #[test]
    fn width_parses_to_some() {
        let sheet = css::parse("div { width: 200px; height: auto }");
        let doc = html::parse(r#"<html><body><div>t</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        assert_eq!(map[&d].width, Some(200.0));
        assert_eq!(map[&d].height, None);
    }

    #[test]
    fn percentage_and_garbage_ignored() {
        assert_eq!(parse_length("50%"), None);
        assert_eq!(parse_length("auto"), None);
        assert_eq!(parse_length("garbage"), None);
        assert_eq!(parse_length("12px"), Some(12.0));
        assert_eq!(parse_length("0"), Some(0.0));
    }

    #[test]
    fn display_and_position_parse() {
        let sheet = css::parse(
            "#a { display: flex; position: relative } \
             #b { display: grid } \
             #c { display: inline-block; position: absolute }",
        );
        let doc = html::parse(
            r#"<html><body><div id="a"></div><div id="b"></div><span id="c"></span></body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let a = elem(&doc, |e| e.id() == Some("a"));
        let b = elem(&doc, |e| e.id() == Some("b"));
        let c = elem(&doc, |e| e.id() == Some("c"));
        assert_eq!(map[&a].display, Display::Flex);
        assert_eq!(map[&a].position, Position::Relative);
        assert_eq!(map[&b].display, Display::Grid);
        assert_eq!(map[&c].display, Display::InlineBlock);
        assert_eq!(map[&c].position, Position::Absolute);
    }

    #[test]
    fn display_default_per_tag() {
        let doc = html::parse(r#"<html><body><div></div><span></span></body></html>"#);
        let map = cascade(&doc, &[]);
        let div = elem(&doc, |e| e.tag == "div");
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&div].display, Display::Block);
        assert!(map[&div].display_block);
        assert_eq!(map[&span].display, Display::Inline);
        assert!(!map[&span].display_block);
    }

    #[test]
    fn flex_shorthand_expands() {
        assert_eq!(parse_flex_test("1"), (1.0, 1.0, Some(0.0)));
        assert_eq!(parse_flex_test("2 3 40px"), (2.0, 3.0, Some(40.0)));
        assert_eq!(parse_flex_test("none"), (0.0, 0.0, None));
        assert_eq!(parse_flex_test("auto"), (1.0, 1.0, None));
        assert_eq!(parse_flex_test("0 0 100px"), (0.0, 0.0, Some(100.0)));
    }

    fn parse_flex_test(v: &str) -> (f32, f32, Option<f32>) {
        let mut s = ComputedStyle::default();
        apply_flex_shorthand(&mut s, v);
        (s.flex_grow, s.flex_shrink, s.flex_basis)
    }

    #[test]
    fn flex_grow_and_basis_longhand() {
        let sheet = css::parse("#a { flex-grow: 2; flex-basis: 50px }");
        let doc = html::parse(r#"<html><body><div id="a"></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let a = elem(&doc, |e| e.id() == Some("a"));
        assert_eq!(map[&a].flex_grow, 2.0);
        assert_eq!(map[&a].flex_basis, Some(50.0));
        assert_eq!(map[&a].flex_shrink, 1.0); // default
    }

    #[test]
    fn gap_one_and_two_values() {
        assert_eq!(parse_gap("10px"), Some((10.0, 10.0)));
        assert_eq!(parse_gap("10px 20px"), Some((10.0, 20.0)));
    }

    #[test]
    fn grid_template_columns_track_parsing() {
        assert_eq!(
            parse_track_list("100px 1fr 50% auto"),
            vec![
                TrackSize::Px(100.0),
                TrackSize::Fr(1.0),
                TrackSize::Pct(50.0),
                TrackSize::Auto
            ]
        );
        // repeat() expansion.
        assert_eq!(
            parse_track_list("repeat(3, 1fr)"),
            vec![TrackSize::Fr(1.0), TrackSize::Fr(1.0), TrackSize::Fr(1.0)]
        );
    }

    #[test]
    fn insets_and_z_index_parse() {
        let sheet =
            css::parse("#a { top: 10px; left: 20px; right: auto; bottom: 5px; z-index: 7 }");
        let doc = html::parse(r#"<html><body><div id="a"></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let a = elem(&doc, |e| e.id() == Some("a"));
        assert_eq!(map[&a].top, Some(10.0));
        assert_eq!(map[&a].left, Some(20.0));
        assert_eq!(map[&a].right, None); // auto
        assert_eq!(map[&a].bottom, Some(5.0));
        assert_eq!(map[&a].z_index, Some(7));
    }

    #[test]
    fn grid_placement_parses() {
        assert_eq!(
            parse_grid_placement("1 / 3"),
            Some(GridPlacement { start: Some(1), end: GridEnd::Line(3) })
        );
        assert_eq!(
            parse_grid_placement("2 / span 2"),
            Some(GridPlacement { start: Some(2), end: GridEnd::Span(2) })
        );
        assert_eq!(
            parse_grid_placement("span 3"),
            Some(GridPlacement { start: None, end: GridEnd::Span(3) })
        );
    }

    #[test]
    fn rgb_function_parses() {
        assert_eq!(parse_color("rgb(255 0 0)"), Some((255, 0, 0)));
        assert_eq!(parse_color("rgb(255, 0, 0)"), Some((255, 0, 0)));
        assert_eq!(parse_color("rgba(0, 0, 255, 0.5)"), Some((0, 0, 255)));
        assert_eq!(parse_color("rgb(100% 0% 0%)"), Some((255, 0, 0)));
    }

    #[test]
    fn hsl_function_parses_to_red() {
        let (r, g, b) = parse_color("hsl(0 100% 50%)").unwrap();
        assert!(r > 250, "r={r}");
        assert!(g < 5 && b < 5, "g={g} b={b}");
    }

    #[test]
    fn oklch_red_is_roughly_red() {
        // Tailwind-ish red: high lightness/chroma at ~29deg hue.
        let (r, g, b) = parse_color("oklch(0.628 0.2577 29.23)").unwrap();
        assert!(r > 200, "expected high R, got {r}");
        assert!(g < 120 && b < 120, "expected low-ish G/B, got g={g} b={b}");
        assert!(r > g && r > b, "red should dominate: {r},{g},{b}");
    }

    #[test]
    fn oklab_parses() {
        // Should not panic and stay in range.
        let c = parse_color("oklab(0.5 0.1 0.1)");
        assert!(c.is_some());
    }

    #[test]
    fn hex_alpha_drops_alpha() {
        assert_eq!(parse_color("#ff000080"), Some((255, 0, 0)));
        assert_eq!(parse_color("#f008"), Some((255, 0, 0)));
    }

    #[test]
    fn transparent_yields_no_color() {
        assert_eq!(parse_color("transparent"), None);
    }

    #[test]
    fn var_resolves_from_root_to_descendant() {
        // :root sets --x; a descendant uses color: var(--x).
        let sheet = css::parse(":root { --x: #0000ff } span { color: var(--x) }");
        let doc = html::parse(r#"<html><body><div><span>t</span></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&span].color, (0, 0, 255));
    }

    #[test]
    fn var_fallback_used_when_undefined() {
        let sheet = css::parse("p { color: var(--missing, #00ff00) }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (0, 255, 0));
    }

    #[test]
    fn var_referencing_var_resolves() {
        let sheet = css::parse(":root { --a: #ff0000; --b: var(--a) } p { color: var(--b) }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (255, 0, 0));
    }

    #[test]
    fn cyclic_var_does_not_hang() {
        let sheet = css::parse(":root { --a: var(--b); --b: var(--a) } p { color: var(--a) }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        // Should terminate (depth cap) and simply not set a color.
        let _ = cascade(&doc, &[sheet]);
    }

    #[test]
    fn current_color_uses_element_color() {
        let sheet = css::parse("p { color: #ff0000; border: 1px solid currentColor }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].border_color, (255, 0, 0));
    }

    #[test]
    fn inherit_keyword_takes_parent_color() {
        let sheet = css::parse("#wrap { color: #ff0000 } span { color: inherit }");
        let doc = html::parse(
            r#"<html><body><div id="wrap"><span>t</span></div></body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&span].color, (255, 0, 0));
    }

    #[test]
    fn media_min_width_rule_applies_at_desktop() {
        // min-width:768px applies at the assumed 1280px viewport.
        let sheet = css::parse("@media (min-width: 768px) { p { color: #00ff00 } }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (0, 255, 0));
    }

    #[test]
    fn media_min_width_above_viewport_does_not_apply() {
        let sheet = css::parse(
            "p { color: #ff0000 } @media (min-width: 2000px) { p { color: #00ff00 } }",
        );
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        // 2000px > 1280px assumed width, so the media rule does not apply: stays red.
        assert_eq!(map[&p].color, (255, 0, 0));
    }

    #[test]
    fn media_prefers_color_scheme_tracks_os_appearance() {
        let _g = scheme_guard();
        let sheet = css::parse(
            "p { color: rgb(10,20,30) } \
             @media (prefers-color-scheme: dark) { p { color: rgb(1,2,3) } } \
             @media (prefers-color-scheme: light) { p { color: rgb(4,5,6) } }",
        );
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);

        // Dark: the `dark` rule applies, the `light` rule is dropped.
        set_color_scheme_dark(true);
        let map = cascade(&doc, &[sheet.clone()]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (1, 2, 3), "dark rule should win in Dark mode");

        // Light: the `light` rule applies, the `dark` rule is dropped.
        set_color_scheme_dark(false);
        let map = cascade(&doc, &[sheet]);
        assert_eq!(map[&p].color, (4, 5, 6), "light rule should win in Light mode");
    }

    #[test]
    fn color_scheme_parses() {
        assert_eq!(parse_color_scheme("normal"), Some(ColorScheme::Normal));
        assert_eq!(parse_color_scheme("light"), Some(ColorScheme::Light));
        assert_eq!(parse_color_scheme("dark"), Some(ColorScheme::Dark));
        assert_eq!(parse_color_scheme("light dark"), Some(ColorScheme::LightDark));
        assert_eq!(parse_color_scheme("dark light"), Some(ColorScheme::LightDark));
        // `only` and unknown idents are ignored.
        assert_eq!(parse_color_scheme("only dark"), Some(ColorScheme::Dark));
        assert_eq!(parse_color_scheme("dark only"), Some(ColorScheme::Dark));
        assert_eq!(parse_color_scheme("foo bar"), Some(ColorScheme::Normal));
        assert_eq!(parse_color_scheme(""), None);
    }

    #[test]
    fn color_scheme_resolves_dark() {
        assert!(ColorScheme::Dark.resolves_dark(false));
        assert!(ColorScheme::Dark.resolves_dark(true));
        assert!(!ColorScheme::Light.resolves_dark(true));
        assert!(!ColorScheme::Normal.resolves_dark(true));
        assert!(ColorScheme::LightDark.resolves_dark(true));
        assert!(!ColorScheme::LightDark.resolves_dark(false));
    }

    #[test]
    fn root_dark_scheme_themes_default_text() {
        let _g = scheme_guard();
        // :root { color-scheme: dark } → root used scheme dark → default UA text light. The map's
        // colors are captured during the cascade (which holds CASCADE_LOCK, so the root-scheme
        // global it writes is the one it reads back), so they're race-free to assert on.
        let sheet = css::parse(":root { color-scheme: dark }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        set_color_scheme_dark(false);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        // Default (UA) text color is now light, not black.
        assert_eq!(map[&p].color, (0xe8, 0xe8, 0xe8));
    }

    #[test]
    fn root_light_scheme_keeps_black_text() {
        let _g = scheme_guard();
        let sheet = css::parse(":root { color-scheme: light }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        set_color_scheme_dark(true); // OS dark, but page opts only into light
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (0, 0, 0));
    }

    #[test]
    fn meta_color_scheme_dark_opts_in() {
        let _g = scheme_guard();
        // <meta name="color-scheme" content="dark"> with no CSS property.
        let doc = html::parse(
            r#"<html><head><meta name="color-scheme" content="dark"></head><body><p>t</p></body></html>"#,
        );
        set_color_scheme_dark(false);
        let map = cascade(&doc, &[]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (0xe8, 0xe8, 0xe8));
    }

    #[test]
    fn color_scheme_get_property_serializes() {
        let mut s = ComputedStyle::default();
        s.color_scheme = ColorScheme::LightDark;
        assert_eq!(s.get_property("color-scheme"), "light dark");
        s.color_scheme = ColorScheme::Dark;
        assert_eq!(s.get_property("color-scheme"), "dark");
    }

    #[test]
    fn media_max_width_below_viewport_does_not_apply() {
        let sheet = css::parse(
            "p { color: #ff0000 } @media (max-width: 600px) { p { color: #00ff00 } }",
        );
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (255, 0, 0));
    }

    #[test]
    fn min_max_width_height_parse_px_and_pct() {
        let sheet = css::parse(
            "#a { max-width: 200px; min-width: 50%; max-height: none; min-height: 30px }",
        );
        let doc = html::parse(r#"<html><body><div id="a"></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let a = elem(&doc, |e| e.id() == Some("a"));
        assert_eq!(map[&a].max_width, Some(SizeConstraint::Px(200.0)));
        assert_eq!(map[&a].min_width, Some(SizeConstraint::Pct(0.5)));
        assert_eq!(map[&a].max_height, None); // none → unset
        assert_eq!(map[&a].min_height, Some(SizeConstraint::Px(30.0)));
    }

    #[test]
    fn inset_shorthand_sets_four_sides() {
        let sheet = css::parse("#a { inset: 1px 2px 3px 4px }");
        let doc = html::parse(r#"<html><body><div id="a"></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let a = elem(&doc, |e| e.id() == Some("a"));
        assert_eq!(map[&a].top, Some(1.0));
        assert_eq!(map[&a].right, Some(2.0));
        assert_eq!(map[&a].bottom, Some(3.0));
        assert_eq!(map[&a].left, Some(4.0));
    }

    #[test]
    fn inset_block_and_inline_map_to_physical() {
        let sheet = css::parse("#a { inset-block: 5px 6px; inset-inline: 7px 8px }");
        let doc = html::parse(r#"<html><body><div id="a"></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let a = elem(&doc, |e| e.id() == Some("a"));
        assert_eq!(map[&a].top, Some(5.0));
        assert_eq!(map[&a].bottom, Some(6.0));
        assert_eq!(map[&a].left, Some(7.0));
        assert_eq!(map[&a].right, Some(8.0));
    }

    #[test]
    fn padding_and_margin_block_inline() {
        let sheet = css::parse(
            "#a { padding-block: 4px; padding-inline: 8px 12px; margin-block: 2px 3px }",
        );
        let doc = html::parse(r#"<html><body><div id="a"></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let a = elem(&doc, |e| e.id() == Some("a"));
        assert_eq!(map[&a].padding.top, 4.0);
        assert_eq!(map[&a].padding.bottom, 4.0);
        assert_eq!(map[&a].padding.left, 8.0);
        assert_eq!(map[&a].padding.right, 12.0);
        assert_eq!(map[&a].margin.top, 2.0);
        assert_eq!(map[&a].margin.bottom, 3.0);
    }

    #[test]
    fn line_height_unitless_px_percent() {
        // unitless 1.5 × 16 = 24
        assert_eq!(parse_line_height("1.5", 16.0), Some(24.0));
        // px direct
        assert_eq!(parse_line_height("20px", 16.0), Some(20.0));
        // percent of font-size: 150% × 20 = 30
        assert_eq!(parse_line_height("150%", 20.0), Some(30.0));
        // em × font-size
        assert_eq!(parse_line_height("2em", 10.0), Some(20.0));
        assert_eq!(parse_line_height("normal", 16.0), None);
    }

    #[test]
    fn line_height_inherits_resolved_px() {
        let sheet = css::parse("#wrap { font-size: 20px; line-height: 1.5 }");
        let doc = html::parse(
            r#"<html><body><div id="wrap"><span>t</span></div></body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let wrap = elem(&doc, |e| e.id() == Some("wrap"));
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&wrap].line_height, Some(30.0)); // 1.5 × 20
        assert_eq!(map[&span].line_height, Some(30.0)); // inherited resolved px
    }

    #[test]
    fn text_transform_parses_and_inherits() {
        let sheet = css::parse("#wrap { text-transform: uppercase }");
        let doc = html::parse(
            r#"<html><body><div id="wrap"><span>t</span></div></body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let wrap = elem(&doc, |e| e.id() == Some("wrap"));
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&wrap].text_transform, TextTransform::Uppercase);
        assert_eq!(map[&span].text_transform, TextTransform::Uppercase);
    }

    #[test]
    fn text_decoration_underline_flag() {
        let sheet = css::parse("#a { text-decoration: underline } #b { text-decoration: line-through } #c { text-decoration: none }");
        let doc = html::parse(
            r#"<html><body><a id="a">x</a><a id="b">y</a><a id="c">z</a></body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let a = elem(&doc, |e| e.id() == Some("a"));
        let b = elem(&doc, |e| e.id() == Some("b"));
        let c = elem(&doc, |e| e.id() == Some("c"));
        assert!(map[&a].underline);
        assert!(!map[&a].line_through);
        assert!(map[&b].line_through);
        assert!(!map[&c].underline && !map[&c].line_through);
    }

    #[test]
    fn ua_inline_text_defaults_cascade() {
        // The UA stylesheet styles inline text elements; verify a representative set reaches
        // the computed style (and is reported by getComputedStyle).
        let doc = html::parse(
            r##"<html><body>
                 <a href="#">link</a>
                 <s>strike</s>
                 <del>del</del>
                 <ins>ins</ins>
                 <mark>mark</mark>
                 <cite>cite</cite>
                 <abbr title="t">abbr</abbr>
                 <sup>2</sup>
                 <sub>2</sub>
                 <small>small</small>
               </body></html>"##,
        );
        let map = cascade(&doc, &[]);
        let g = |tag: &str| {
            let id = elem(&doc, |e| e.tag == tag);
            &map[&id]
        };
        // <a>: blue + underline.
        let a = g("a");
        assert!(a.underline, "a should be underlined");
        assert_eq!(a.color, (0x00, 0x00, 0xee), "a should be link blue");
        assert_eq!(a.get_property("text-decoration"), "underline");
        // <s>/<del>: line-through.
        assert!(g("s").line_through);
        assert!(g("del").line_through);
        assert_eq!(g("s").get_property("text-decoration"), "line-through");
        // <ins>: underline.
        assert!(g("ins").underline);
        // <mark>: yellow bg, black text.
        assert_eq!(g("mark").background_color, Some((0xff, 0xff, 0x00)));
        assert_eq!(g("mark").color, (0, 0, 0));
        assert_eq!(g("mark").get_property("background-color"), "rgb(255, 255, 0)");
        // <cite>: italic.
        assert!(g("cite").italic);
        // <abbr title>: underline.
        assert!(g("abbr").underline);
        // <sup>/<sub>: smaller font + vertical-align.
        assert!(g("sup").font_size < 16.0, "sup should be smaller, got {}", g("sup").font_size);
        assert_eq!(g("sup").vertical_align, VerticalAlign::Super);
        assert_eq!(g("sub").vertical_align, VerticalAlign::Sub);
        assert_eq!(g("sup").get_property("vertical-align"), "super");
        // <small>: smaller font.
        assert!(g("small").font_size < 16.0, "small should be smaller, got {}", g("small").font_size);
    }

    #[test]
    fn font_size_relative_keywords() {
        assert_eq!(parse_font_size("smaller", 16.0), Some(16.0 / 1.2));
        assert_eq!(parse_font_size("larger", 10.0), Some(12.0));
    }

    #[test]
    fn q_quote_marks_via_pseudo_content() {
        let doc = html::parse(r#"<html><body><q>quote</q></body></html>"#);
        let map = cascade(&doc, &[]);
        let q = elem(&doc, |e| e.tag == "q");
        let before = map[&q].before.as_ref().expect("q::before should exist");
        let after = map[&q].after.as_ref().expect("q::after should exist");
        assert_eq!(before.content.as_deref(), Some("\u{201C}"));
        assert_eq!(after.content.as_deref(), Some("\u{201D}"));
    }

    #[test]
    fn opacity_clamps_to_unit_range() {
        let sheet = css::parse("#a { opacity: 0.5 } #b { opacity: 2 } #c { opacity: -1 }");
        let doc = html::parse(
            r#"<html><body><div id="a"></div><div id="b"></div><div id="c"></div></body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let a = elem(&doc, |e| e.id() == Some("a"));
        let b = elem(&doc, |e| e.id() == Some("b"));
        let c = elem(&doc, |e| e.id() == Some("c"));
        assert_eq!(map[&a].opacity, 0.5);
        assert_eq!(map[&b].opacity, 1.0);
        assert_eq!(map[&c].opacity, 0.0);
    }

    #[test]
    fn border_radius_one_and_four_values() {
        assert_eq!(parse_border_radius("8px"), Some(8.0));
        // four values → first is used uniformly
        assert_eq!(parse_border_radius("4px 8px 12px 16px"), Some(4.0));
        // elliptical syntax: use horizontal radii before `/`
        assert_eq!(parse_border_radius("10px / 20px"), Some(10.0));
    }

    #[test]
    fn opacity_does_not_inherit() {
        let sheet = css::parse("#wrap { opacity: 0.5 }");
        let doc = html::parse(
            r#"<html><body><div id="wrap"><span>t</span></div></body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&span].opacity, 1.0);
    }

    // --- Math functions: min/max/clamp/calc -------------------------------------------------

    #[test]
    fn eval_min_max_clamp() {
        assert_eq!(eval_length("min(10px, 20px)", 16.0), Some(10.0));
        assert_eq!(eval_length("max(10px, 20px, 5px)", 16.0), Some(20.0));
        // clamped up to lo
        assert_eq!(eval_length("clamp(5px, 2px, 10px)", 16.0), Some(5.0));
        // value within range
        assert_eq!(eval_length("clamp(5px, 8px, 10px)", 16.0), Some(8.0));
        // clamped down to hi
        assert_eq!(eval_length("clamp(5px, 80px, 10px)", 16.0), Some(10.0));
    }

    #[test]
    fn eval_calc_precedence_and_units() {
        // 2rem(32) + 10px = 42
        assert_eq!(eval_length("calc(2rem + 10px)", 16.0), Some(42.0));
        // precedence: 2 + 3*4px = 14
        assert_eq!(eval_length("calc(2px + 3 * 4px)", 16.0), Some(14.0));
        // parens override precedence: (2 + 3) * 4 = 20
        assert_eq!(eval_length("calc((2px + 3px) * 4)", 16.0), Some(20.0));
        // em resolves against the passed font size
        assert_eq!(eval_length("calc(2em)", 10.0), Some(20.0));
        // vw = 1280/100 * 10 = 128
        assert_eq!(eval_length("calc(10vw)", 16.0), Some(128.0));
    }

    #[test]
    fn eval_nested_functions() {
        // calc(1px*100) = 100, clamped to [1rem=16, 50] → 50
        assert_eq!(eval_length("clamp(1rem, calc(1px * 100), 50px)", 16.0), Some(50.0));
        // nested min inside max
        assert_eq!(eval_length("max(min(30px, 10px), 5px)", 16.0), Some(10.0));
    }

    #[test]
    fn eval_unknown_falls_back_to_none() {
        assert_eq!(eval_length("calc(2px + 3foo)", 16.0), None); // unknown unit
        assert_eq!(eval_length("min()", 16.0), None);
        assert_eq!(eval_length("calc(1px /)", 16.0), None); // malformed
        assert_eq!(eval_length("clamp(1px, 2px)", 16.0), None); // wrong arity
    }

    #[test]
    fn plain_lengths_still_parse_identically() {
        assert_eq!(parse_length("12px"), Some(12.0));
        assert_eq!(parse_length("0"), Some(0.0));
        assert_eq!(parse_length("50%"), None);
        // math wired into parse_length
        assert_eq!(parse_length("min(10px, 20px)"), Some(10.0));
        assert_eq!(parse_length("calc(2rem + 10px)"), Some(42.0));
    }

    #[test]
    fn font_size_clamp_resolves_on_node() {
        let sheet = css::parse("p { font-size: clamp(10px, 2vw, 30px) }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        // 2vw = 25.6, within [10,30] → 25.6
        assert!((map[&p].font_size - 25.6).abs() < 0.01, "got {}", map[&p].font_size);
    }

    #[test]
    fn width_calc_resolves_on_node() {
        let sheet = css::parse("div { width: calc(100px + 1rem) }");
        let doc = html::parse(r#"<html><body><div>t</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        assert_eq!(map[&d].width, Some(116.0));
    }

    #[test]
    fn max_width_max_function_resolves() {
        let sheet = css::parse("div { max-width: max(200px, 50px) }");
        let doc = html::parse(r#"<html><body><div>t</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        assert_eq!(map[&d].max_width, Some(SizeConstraint::Px(200.0)));
    }

    // --- Container queries -------------------------------------------------------------------

    #[test]
    fn container_min_width_rule_applies_at_assumed_width() {
        // 400px <= assumed container width (1000px) → applies.
        let sheet = css::parse("@container (min-width: 400px) { p { color: #00ff00 } }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (0, 255, 0));
    }

    #[test]
    fn container_min_width_above_assumed_does_not_apply() {
        let sheet = css::parse(
            "p { color: #ff0000 } @container (min-width: 5000px) { p { color: #00ff00 } }",
        );
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        // 5000px > 1000px assumed container width → rule does not apply: stays red.
        assert_eq!(map[&p].color, (255, 0, 0));
    }

    #[test]
    fn container_unrecognized_condition_is_permissive() {
        // An aspect/orientation-style condition we don't model → rule still applies.
        let sheet = css::parse("@container (orientation: landscape) { p { color: #00ff00 } }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (0, 255, 0));
    }

    #[test]
    fn box_props_do_not_inherit() {
        let sheet = css::parse("#wrap { margin: 30px; padding: 10px; width: 300px }");
        let doc = html::parse(
            r#"<html><body><div id="wrap"><span>child</span></div></body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&span].margin, Edges::default());
        assert_eq!(map[&span].padding, Edges::default());
        assert_eq!(map[&span].width, None);
    }

    /// Brute-force reference: for one element, the set of `(origin, order, max_specificity)`
    /// the *original* O(all-rules) scan would have produced — one entry per rule, max
    /// specificity over its comma selectors, media/container gated, exactly as the pre-index
    /// code did. Used to cross-check the index produces the identical match set.
    fn naive_matches(
        doc: &dom::Document,
        nid: dom::NodeId,
        ua: &css::Stylesheet,
        author: &[css::Stylesheet],
    ) -> Vec<(u8, usize, u32)> {
        let mut out = Vec::new();
        let mut order = 0usize;
        for rule in &ua.rules {
            if media_applies(rule.media.as_deref())
                && container_applies(rule.container.as_deref())
            {
                if let Some(spec) = rule_specificity(&rule.selectors, doc, nid) {
                    out.push((0u8, order, spec));
                }
            }
            order += 1;
        }
        for sheet in author {
            for rule in &sheet.rules {
                if media_applies(rule.media.as_deref())
                    && container_applies(rule.container.as_deref())
                {
                    if let Some(spec) = rule_specificity(&rule.selectors, doc, nid) {
                        out.push((1u8, order, spec));
                    }
                }
                order += 1;
            }
        }
        out.sort();
        out
    }

    /// The same query the indexed cascade runs, surfaced as `(origin, order, max_spec)` so it
    /// can be compared against `naive_matches`.
    fn indexed_matches(
        doc: &dom::Document,
        nid: dom::NodeId,
        el: &dom::ElementData,
        index: &SelectorIndex,
    ) -> Vec<(u8, usize, u32)> {
        let mut best: HashMap<usize, (u8, u32)> = HashMap::new();
        let mut consider = |e: &Entry| {
            if complex_matches(doc, nid, &e.compiled.selector) {
                best.entry(e.order)
                    .and_modify(|(_, s)| *s = (*s).max(e.compiled.specificity))
                    .or_insert((e.origin, e.compiled.specificity));
            }
        };
        if let Some(id) = el.id() {
            if let Some(b) = index.by_id.get(id) {
                for e in b {
                    consider(e);
                }
            }
        }
        for class in el.classes() {
            if let Some(b) = index.by_class.get(class) {
                for e in b {
                    consider(e);
                }
            }
        }
        if let Some(b) = index.by_type.get(&el.tag.to_lowercase()) {
            for e in b {
                consider(e);
            }
        }
        for e in &index.universal {
            consider(e);
        }
        let mut out: Vec<_> =
            best.into_iter().map(|(order, (origin, spec))| (origin, order, spec)).collect();
        out.sort();
        out
    }

    #[test]
    fn indexed_match_set_equals_naive_for_varied_selectors() {
        // Exercise id / class / type / universal / :root / multi-class / comma / div.foo.
        let sheet = css::parse(
            "* { color: #111111 }
             :root { color: #222222 }
             div { color: #333333 }
             .foo { color: #444444 }
             div.foo { color: #555555 }
             .foo.bar { color: #666666 }
             #hero, .promo { color: #777777 }
             #hero { font-size: 20px }
             p, .foo, #hero { letter-spacing: 1px }
             a > b { color: #888888 }
             [data-x] { color: #999999 }",
        );
        let ua = user_agent_stylesheet();
        let author = [sheet];
        let index = SelectorIndex::build(&ua, &author);

        let doc = html::parse(
            r#"<html><body>
                 <div id="hero" class="foo bar promo">A</div>
                 <div class="foo">B</div>
                 <p class="promo">C</p>
                 <span>D</span>
                 <a><b>E</b></a>
               </body></html>"#,
        );
        // Check every element in the tree, not just a handful.
        fn walk(doc: &dom::Document, id: dom::NodeId, out: &mut Vec<dom::NodeId>) {
            if let NodeData::Element(_) = &doc.get(id).data {
                out.push(id);
            }
            for &c in &doc.get(id).children {
                walk(doc, c, out);
            }
        }
        let mut ids = Vec::new();
        walk(&doc, doc.root(), &mut ids);
        assert!(ids.len() >= 7);
        for id in ids {
            if let NodeData::Element(el) = &doc.get(id).data {
                assert_eq!(
                    indexed_matches(&doc, id, el, &index),
                    naive_matches(&doc, id, &ua, &author),
                    "match set diverged for <{}>",
                    el.tag
                );
            }
        }
    }

    #[test]
    fn varied_selector_cascade_values() {
        let sheet = css::parse(
            ":root { color: #010101 }
             * { letter-spacing: 0 }
             div { color: #020202 }
             .foo { color: #030303 }
             div.foo { color: #0a0b0c }
             .foo.bar { font-size: 21px }
             #hero, .promo { font-weight: bold }",
        );
        let doc = html::parse(
            r#"<html><body>
                 <div id="hero" class="foo bar promo">A</div>
                 <div class="foo">B</div>
                 <span class="bar">C</span>
               </body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        // <div id=hero class="foo bar promo">: div.foo (spec 11) beats .foo (10) and div (1)
        // → color #0a0b0c; .foo.bar sets font-size 21; #hero/.promo → bold.
        let hero = elem(&doc, |e| e.id() == Some("hero"));
        assert_eq!(map[&hero].color, (10, 11, 12));
        assert_eq!(map[&hero].font_size, 21.0);
        assert!(map[&hero].bold);
        // <div class="foo">: div.foo doesn't match (needs tag div — it does), wait it's a div
        // so div.foo matches → #0a0b0c too.
        // <span class="bar">: only `*` and `.foo.bar` (no, needs foo) — none color it, so it
        // inherits the html/body UA color.
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&span].color, (0, 0, 0));
        assert!(!span_is_bold(&map, &doc));
    }

    fn span_is_bold(
        map: &HashMap<dom::NodeId, ComputedStyle>,
        doc: &dom::Document,
    ) -> bool {
        let span = elem(doc, |e| e.tag == "span");
        map[&span].bold
    }

    // ====================================================================================
    // Complex selector engine tests
    // ====================================================================================

    /// Find the nth (0-based) element matching a predicate, depth-first.
    fn elem_nth(
        doc: &dom::Document,
        n: usize,
        pred: impl Fn(&dom::ElementData) -> bool,
    ) -> dom::NodeId {
        fn walk(
            doc: &dom::Document,
            id: dom::NodeId,
            pred: &dyn Fn(&dom::ElementData) -> bool,
            out: &mut Vec<dom::NodeId>,
        ) {
            if let NodeData::Element(e) = &doc.get(id).data {
                if pred(e) {
                    out.push(id);
                }
            }
            for &c in &doc.get(id).children {
                walk(doc, c, pred, out);
            }
        }
        let mut out = Vec::new();
        walk(doc, doc.root(), &pred, &mut out);
        out[n]
    }

    fn red() -> (u8, u8, u8) {
        (255, 0, 0)
    }

    #[test]
    fn descendant_combinator() {
        let sheet = css::parse(".a .b { color: red }");
        let doc = html::parse(
            r#"<html><body>
                 <div class="a"><div><span class="b">x</span></div></div>
                 <span class="b">y</span>
               </body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let inside = elem_nth(&doc, 0, |e| e.classes().any(|c| c == "b"));
        let outside = elem_nth(&doc, 1, |e| e.classes().any(|c| c == "b"));
        assert_eq!(map[&inside].color, red());
        assert_ne!(map[&outside].color, red());
    }

    #[test]
    fn child_combinator() {
        let sheet = css::parse(".a > .b { color: red }");
        let doc = html::parse(
            r#"<html><body>
                 <div class="a"><span class="b">direct</span></div>
                 <div class="a"><div><span class="b">grand</span></div></div>
               </body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let direct = elem_nth(&doc, 0, |e| e.classes().any(|c| c == "b"));
        let grand = elem_nth(&doc, 1, |e| e.classes().any(|c| c == "b"));
        assert_eq!(map[&direct].color, red());
        assert_ne!(map[&grand].color, red());
    }

    #[test]
    fn adjacent_sibling_combinator() {
        let sheet = css::parse(".a + .b { color: red }");
        let doc = html::parse(
            r#"<html><body>
                 <span class="a">a</span><span class="b">adjacent</span>
                 <span class="x">gap</span><span class="b">notadjacent</span>
               </body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let adj = elem_nth(&doc, 0, |e| e.classes().any(|c| c == "b"));
        let notadj = elem_nth(&doc, 1, |e| e.classes().any(|c| c == "b"));
        assert_eq!(map[&adj].color, red());
        assert_ne!(map[&notadj].color, red());
    }

    #[test]
    fn general_sibling_combinator() {
        let sheet = css::parse(".a ~ .b { color: red }");
        let doc = html::parse(
            r#"<html><body>
                 <span class="a">a</span><span class="x">x</span><span class="b">after</span>
                 <div><span class="b">nested-before-no-a</span></div>
               </body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let after = elem_nth(&doc, 0, |e| e.classes().any(|c| c == "b"));
        let nested = elem_nth(&doc, 1, |e| e.classes().any(|c| c == "b"));
        assert_eq!(map[&after].color, red());
        assert_ne!(map[&nested].color, red());
    }

    #[test]
    fn nth_child_and_structural() {
        let sheet = css::parse(
            "li:nth-child(2) { color: red }
             li:first-child { font-weight: bold }
             li:last-child { font-style: italic }
             li:nth-child(odd) { letter-spacing: 3px }",
        );
        let doc = html::parse(
            r#"<html><body><ul><li>1</li><li>2</li><li>3</li></ul></body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let li1 = elem_nth(&doc, 0, |e| e.tag == "li");
        let li2 = elem_nth(&doc, 1, |e| e.tag == "li");
        let li3 = elem_nth(&doc, 2, |e| e.tag == "li");
        assert_eq!(map[&li2].color, red()); // nth-child(2)
        assert!(map[&li1].bold); // first-child
        assert!(map[&li3].italic); // last-child
        assert_eq!(map[&li1].letter_spacing, 3.0); // odd → 1
        assert_eq!(map[&li3].letter_spacing, 3.0); // odd → 3
        assert_eq!(map[&li2].letter_spacing, 0.0); // even → not odd
    }

    #[test]
    fn only_child_and_of_type() {
        let sheet = css::parse(
            "p:only-child { color: red }
             span:first-of-type { font-weight: bold }",
        );
        let doc = html::parse(
            r#"<html><body>
                 <div><p>solo</p></div>
                 <div><p>a</p><p>b</p></div>
                 <div><span>s1</span><em>e</em><span>s2</span></div>
               </body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let solo = elem_nth(&doc, 0, |e| e.tag == "p");
        let paired = elem_nth(&doc, 1, |e| e.tag == "p");
        assert_eq!(map[&solo].color, red());
        assert_ne!(map[&paired].color, red());
        let s1 = elem_nth(&doc, 0, |e| e.tag == "span");
        let s2 = elem_nth(&doc, 1, |e| e.tag == "span");
        assert!(map[&s1].bold);
        assert!(!map[&s2].bold);
    }

    #[test]
    fn attribute_selectors() {
        let sheet = css::parse(
            "[data-x] { color: red }
             input[type=text] { font-weight: bold }
             a[href^=\"https\"] { font-style: italic }
             [class~=foo] { letter-spacing: 2px }
             [type=TEXT i] { text-decoration: underline }",
        );
        let doc = html::parse(
            r#"<html><body>
                 <div data-x="1">x</div>
                 <input type="text">
                 <a href="https://example.com">link</a>
                 <a href="http://nope.com">nope</a>
                 <span class="foo bar">word</span>
                 <input type="TEXT">
               </body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let dx = elem_nth(&doc, 0, |e| e.attrs.contains_key("data-x"));
        assert_eq!(map[&dx].color, red());
        let inp = elem_nth(&doc, 0, |e| e.tag == "input");
        assert!(map[&inp].bold);
        let a_https = elem_nth(&doc, 0, |e| e.tag == "a");
        let a_http = elem_nth(&doc, 1, |e| e.tag == "a");
        assert!(map[&a_https].italic);
        assert!(!map[&a_http].italic);
        let foo = elem_nth(&doc, 0, |e| e.classes().any(|c| c == "foo"));
        assert_eq!(map[&foo].letter_spacing, 2.0);
        // case-insensitive [type=TEXT i] matches both lowercase and uppercase type.
        let inp_upper = elem_nth(&doc, 1, |e| e.tag == "input");
        assert!(map[&inp].underline);
        assert!(map[&inp_upper].underline);
    }

    #[test]
    fn state_checked_and_disabled() {
        let sheet = css::parse(
            "input:checked { color: red }
             button:disabled { font-weight: bold }
             input:enabled { font-style: italic }",
        );
        let doc = html::parse(
            r#"<html><body>
                 <input type="checkbox" checked>
                 <input type="checkbox">
                 <button disabled>b</button>
               </body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let checked = elem_nth(&doc, 0, |e| e.tag == "input");
        let unchecked = elem_nth(&doc, 1, |e| e.tag == "input");
        assert_eq!(map[&checked].color, red());
        assert_ne!(map[&unchecked].color, red());
        assert!(map[&unchecked].italic); // :enabled (no disabled attr)
        let btn = elem_nth(&doc, 0, |e| e.tag == "button");
        assert!(map[&btn].bold);
    }

    #[test]
    fn hover_and_focus_via_interaction_state() {
        let sheet = css::parse(
            ".btn:hover { color: red }
             .field:focus { font-weight: bold }",
        );
        let doc = html::parse(
            r#"<html><body>
                 <a class="btn"><span>label</span></a>
                 <input class="field">
               </body></html>"#,
        );
        let btn = elem(&doc, |e| e.classes().any(|c| c == "btn"));
        let label = elem(&doc, |e| e.tag == "span");
        let field = elem(&doc, |e| e.classes().any(|c| c == "field"));

        // Hover the inner span: `.btn:hover` should match the ancestor `.btn` too.
        set_interaction_state(Some(label.0), None);
        let map = cascade(&doc, &[sheet.clone()]);
        assert_eq!(map[&btn].color, red());

        // Focus the field.
        set_interaction_state(None, Some(field.0));
        let map = cascade(&doc, &[sheet.clone()]);
        assert!(map[&field].bold);

        // Clear state: neither matches.
        set_interaction_state(None, None);
        let map = cascade(&doc, &[sheet]);
        assert_ne!(map[&btn].color, red());
        assert!(!map[&field].bold);
    }

    #[test]
    fn functional_not_is_where() {
        let sheet = css::parse(
            "div:not(.x) { color: red }
             :is(.a, .b) { font-weight: bold }
             :where(.c) { font-style: italic }",
        );
        let doc = html::parse(
            r#"<html><body>
                 <div>plain</div>
                 <div class="x">excluded</div>
                 <div class="a">isa</div>
                 <div class="c">wherec</div>
               </body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let plain = elem_nth(&doc, 0, |e| e.tag == "div");
        let excluded = elem_nth(&doc, 0, |e| e.tag == "div" && e.classes().any(|c| c == "x"));
        let isa = elem(&doc, |e| e.classes().any(|c| c == "a"));
        let wherec = elem(&doc, |e| e.classes().any(|c| c == "c"));
        assert_eq!(map[&plain].color, red()); // :not(.x) matches a plain div
        assert_ne!(map[&excluded].color, red()); // .x excluded
        assert!(map[&isa].bold); // :is(.a, .b)
        assert!(map[&wherec].italic); // :where(.c)
    }

    #[test]
    fn specificity_id_class_type_and_not() {
        // #id beats .cls beats tag.
        assert!(
            compile_selector("#x").unwrap().specificity
                > compile_selector(".c").unwrap().specificity
        );
        assert!(
            compile_selector(".c").unwrap().specificity
                > compile_selector("p").unwrap().specificity
        );
        // :not(#x) carries id-level specificity.
        assert_eq!(
            compile_selector("div:not(#x)").unwrap().specificity,
            compile_selector("#x").unwrap().specificity
                + compile_selector("div").unwrap().specificity
        );
        // :where() contributes ZERO specificity (only the type counts here).
        assert_eq!(
            compile_selector("div:where(.a.b.c)").unwrap().specificity,
            compile_selector("div").unwrap().specificity
        );
    }

    #[test]
    fn where_zero_specificity_loses_to_class() {
        // `:where(.hi)` adds 0 specificity, so a plain `.lo` (class) should win on source order
        // when both target the same element and `.lo` comes later.
        let sheet = css::parse(
            ":where(.hi) { color: blue }
             .lo { color: red }",
        );
        let doc = html::parse(r#"<html><body><p class="hi lo">t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        // Equal specificity (0 vs 10) → .lo (10) wins → red.
        assert_eq!(map[&p].color, red());
    }

    #[test]
    fn pseudo_element_does_not_apply_to_originating_element() {
        // `::before { color: red }` styles the pseudo, NOT the element: `p` itself stays blue.
        // And with no `content`, no pseudo box is generated at all.
        let sheet = css::parse(
            "p::before { color: red }
             p { color: blue }",
        );
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (0, 0, 255)); // only `p { blue }` applied to the element
        assert!(map[&p].before.is_none()); // no `content` → no generated box
        // The compile step now KEEPS pseudo-elements (routing them to ::before/::after).
        assert_eq!(
            compile_selector("p::before").unwrap().pseudo_element,
            Some(PseudoElement::Before)
        );
        assert_eq!(
            compile_selector("div::after").unwrap().pseudo_element,
            Some(PseudoElement::After)
        );
    }

    #[test]
    fn pseudo_element_before_generates_content() {
        let sheet = css::parse(r#".x::before { content: "→" } p { color: blue }"#);
        let doc = html::parse(r#"<html><body><div class="x">hi</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        let before = map[&d].before.as_ref().expect("::before box");
        assert_eq!(before.content.as_deref(), Some("→"));
        assert!(map[&d].after.is_none());
    }

    #[test]
    fn pseudo_element_empty_and_none_generate_no_or_empty_box() {
        let sheet = css::parse(
            r#"div::after { content: "" }
               span::after { content: none }"#,
        );
        let doc = html::parse(r#"<html><body><div>d</div><span>s</span></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        let s = elem(&doc, |e| e.tag == "span");
        // Empty string → a box with empty content (still Some, so styling could show).
        assert_eq!(map[&d].after.as_ref().unwrap().content.as_deref(), Some(""));
        // `content: none` → no box at all.
        assert!(map[&s].after.is_none());
    }

    #[test]
    fn pseudo_element_content_attr() {
        let sheet = css::parse("div::before { content: attr(data-label) }");
        let doc = html::parse(
            r#"<html><body><div data-label="Note">x</div></body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        assert_eq!(map[&d].before.as_ref().unwrap().content.as_deref(), Some("Note"));
    }

    #[test]
    fn pseudo_element_carries_distinct_paint_style() {
        let sheet = css::parse(
            r#"div { color: rgb(0,0,255) }
               div::before { content: "x"; color: rgb(255,0,0); background-color: rgb(0,255,0) }"#,
        );
        let doc = html::parse(r#"<html><body><div>d</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        assert_eq!(map[&d].color, (0, 0, 255)); // element stays blue
        let before = map[&d].before.as_ref().unwrap();
        assert_eq!(before.color, (255, 0, 0)); // pseudo is red
        assert_eq!(before.background_color, Some((0, 255, 0)));
    }

    #[test]
    fn pseudo_element_legacy_single_colon() {
        let sheet = css::parse(r#"div:before { content: "L" }"#);
        let doc = html::parse(r#"<html><body><div>d</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        assert_eq!(map[&d].before.as_ref().unwrap().content.as_deref(), Some("L"));
    }

    #[test]
    fn pseudo_element_specificity_class_beats_type() {
        let sheet = css::parse(
            r#"div::before { content: "a" }
               .x::before { content: "b" }"#,
        );
        let doc = html::parse(r#"<html><body><div class="x">d</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        // `.x::before` (class) wins over `div::before` (type) → "b".
        assert_eq!(map[&d].before.as_ref().unwrap().content.as_deref(), Some("b"));
    }

    #[test]
    fn parse_gcs_pseudo_normalization() {
        use GcsPseudo::*;
        // No leading colon / empty → use the element.
        assert_eq!(parse_gcs_pseudo(""), Element);
        assert_eq!(parse_gcs_pseudo("before"), Element);
        assert_eq!(parse_gcs_pseudo("totallynotapseudo"), Element);
        // Recognized pseudos (both colon forms for the legacy four).
        assert_eq!(parse_gcs_pseudo("::before"), Pseudo("before".into()));
        assert_eq!(parse_gcs_pseudo(":before"), Pseudo("before".into()));
        assert_eq!(parse_gcs_pseudo("::after"), Pseudo("after".into()));
        assert_eq!(parse_gcs_pseudo("::marker"), Pseudo("marker".into()));
        // Functional pseudos.
        assert_eq!(parse_gcs_pseudo("::highlight(name)"), Pseudo("highlight(name)".into()));
        assert_eq!(parse_gcs_pseudo("::highlight( name "), Pseudo("highlight(name)".into())); // auto-closed
        assert_eq!(parse_gcs_pseudo("::picker(select)"), Pseudo("picker(select)".into()));
        // CSS escapes resolve.
        assert_eq!(parse_gcs_pseudo(r":bef\oRE"), Pseudo("before".into()));
        // Invalid forms → empty style.
        assert_eq!(parse_gcs_pseudo("::totallynotapseudo"), Invalid);
        assert_eq!(parse_gcs_pseudo(":totallynotapseudo"), Invalid);
        assert_eq!(parse_gcs_pseudo("::before,"), Invalid);
        assert_eq!(parse_gcs_pseudo("::before@after"), Invalid);
        assert_eq!(parse_gcs_pseudo("::marker"), Pseudo("marker".into()));
        assert_eq!(parse_gcs_pseudo(":marker"), Invalid); // needs double colon
        assert_eq!(parse_gcs_pseudo("::highlight(1)"), Invalid); // arg not an ident
        assert_eq!(parse_gcs_pseudo("::highlight()"), Invalid);
        assert_eq!(parse_gcs_pseudo("::picker(div)"), Invalid); // picker only takes `select`
        assert_eq!(parse_gcs_pseudo("::view-transition-group(*)"), Invalid); // `*` not accepted
    }

    #[test]
    fn compute_pseudo_style_cascades_pseudo_values() {
        let sheet = css::parse(
            r#"#x { color: rgb(0, 0, 1) }
               #x::before { color: red; content: "x" }
               #x::highlight(foo) { color: rgb(0, 128, 0) }"#,
        );
        let doc = html::parse(r#"<html><body><div id="x">d</div></body></html>"#);
        let map = cascade(&doc, &[sheet.clone()]);
        let x = elem(&doc, |e| e.tag == "div");
        let es = &map[&x];
        // ::before: cascaded color + content.
        let before = compute_pseudo_style(&doc, &[sheet.clone()], x, es, "before").unwrap();
        assert_eq!(before.get_property("color"), "rgb(255, 0, 0)");
        assert_eq!(before.get_property("content"), "\"x\"");
        // ::highlight(foo): a named-highlight rule cascades onto the pseudo.
        let hi = compute_pseudo_style(&doc, &[sheet.clone()], x, es, "highlight(foo)").unwrap();
        assert_eq!(hi.get_property("color"), "rgb(0, 128, 0)");
        // A pseudo with no matching rules still yields a (non-empty) style inheriting from the el.
        let marker = compute_pseudo_style(&doc, &[sheet], x, es, "marker").unwrap();
        assert_eq!(marker.get_property("color"), "rgb(0, 0, 1)"); // inherited
        assert!(!marker.property_names().is_empty());
    }

    #[test]
    fn empty_and_root_pseudo() {
        let sheet = css::parse(
            ":root { letter-spacing: 5px }
             p:empty { color: red }",
        );
        let doc = html::parse(
            r#"<html><body><p></p><p>full</p></body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let html_el = elem(&doc, |e| e.tag == "html");
        assert_eq!(map[&html_el].letter_spacing, 5.0);
        let empty_p = elem_nth(&doc, 0, |e| e.tag == "p");
        let full_p = elem_nth(&doc, 1, |e| e.tag == "p");
        assert_eq!(map[&empty_p].color, red());
        assert_ne!(map[&full_p].color, red());
    }

    /// Cross-check: for a doc + sheet exercising combinators/attrs/pseudos, the indexed match set
    /// equals a brute-force `complex_matches` scan over every rule for every element.
    #[test]
    fn indexed_complex_match_set_equals_bruteforce() {
        let sheet = css::parse(
            ".nav a { color: #010101 }
             .card > .title { color: #020202 }
             li:nth-child(2) { color: #030303 }
             a[target=_blank] { color: #040404 }
             input:checked { color: #050505 }
             div:not(.x) { color: #060606 }
             :is(.a, .b) { color: #070707 }
             .a + .b { color: #080808 }
             .a ~ .c { color: #090909 }
             [data-y] { color: #0a0a0a }",
        );
        let ua = user_agent_stylesheet();
        let author = [sheet];
        let index = SelectorIndex::build(&ua, &author);
        let doc = html::parse(
            r#"<html><body>
                 <nav class="nav"><a target="_blank">l</a></nav>
                 <div class="card"><span class="title">t</span></div>
                 <ul><li>1</li><li>2</li></ul>
                 <input type="checkbox" checked>
                 <div class="x">x</div><div>plain</div>
                 <span class="a">a</span><span class="b">b</span><span class="c">c</span>
                 <p data-y="1">y</p>
               </body></html>"#,
        );
        fn walk(doc: &dom::Document, id: dom::NodeId, out: &mut Vec<dom::NodeId>) {
            if let NodeData::Element(_) = &doc.get(id).data {
                out.push(id);
            }
            for &c in &doc.get(id).children {
                walk(doc, c, out);
            }
        }
        let mut ids = Vec::new();
        walk(&doc, doc.root(), &mut ids);
        for id in ids {
            if let NodeData::Element(el) = &doc.get(id).data {
                // Brute-force: scan every rule directly via complex_matches.
                let brute = naive_matches(&doc, id, &ua, &author);
                let indexed = indexed_matches(&doc, id, el, &index);
                assert_eq!(indexed, brute, "match set diverged for <{}>", el.tag);
            }
        }
    }

    // --- get_property (getComputedStyle string serialization) ----------------------------------

    /// Cascade a doc + sheet and return the computed style for the first element matching `pred`.
    fn cs_of(html_src: &str, sheet_src: &str, pred: impl Fn(&dom::ElementData) -> bool) -> ComputedStyle {
        let sheet = css::parse(sheet_src);
        let doc = html::parse(html_src);
        let map = cascade(&doc, &[sheet]);
        let id = elem(&doc, pred);
        map[&id].clone()
    }

    #[test]
    fn get_property_display_block_inline_flex_none() {
        let cs = cs_of("<html><body><div></div></body></html>", "", |e| e.tag == "div");
        assert_eq!(cs.get_property("display"), "block");
        let cs = cs_of("<html><body><span></span></body></html>", "", |e| e.tag == "span");
        assert_eq!(cs.get_property("display"), "inline");
        let cs = cs_of("<html><body><div class='x'></div></body></html>", ".x{display:flex}", |e| e.tag == "div");
        assert_eq!(cs.get_property("display"), "flex");
        let cs = cs_of("<html><body><div class='x'></div></body></html>", ".x{display:none}", |e| e.tag == "div");
        assert_eq!(cs.get_property("display"), "none");
    }

    #[test]
    fn get_property_color_serializes_rgb() {
        let cs = cs_of("<html><body><p style='color:red'>t</p></body></html>", "", |e| e.tag == "p");
        assert_eq!(cs.get_property("color"), "rgb(255, 0, 0)");
    }

    #[test]
    fn get_property_background_color_transparent_default() {
        let cs = cs_of("<html><body><div></div></body></html>", "", |e| e.tag == "div");
        assert_eq!(cs.get_property("background-color"), "rgba(0, 0, 0, 0)");
        let cs = cs_of("<html><body><div style='background-color:#00ff00'></div></body></html>", "", |e| e.tag == "div");
        assert_eq!(cs.get_property("background-color"), "rgb(0, 255, 0)");
    }

    #[test]
    fn get_property_font_size_and_weight() {
        let cs = cs_of("<html><body><p style='font-size:20px;font-weight:bold'>t</p></body></html>", "", |e| e.tag == "p");
        assert_eq!(cs.get_property("font-size"), "20px");
        assert_eq!(cs.get_property("font-weight"), "700");
        let cs = cs_of("<html><body><p>t</p></body></html>", "", |e| e.tag == "p");
        assert_eq!(cs.get_property("font-weight"), "400");
    }

    #[test]
    fn get_property_position() {
        let cs = cs_of("<html><body><div style='position:absolute'></div></body></html>", "", |e| e.tag == "div");
        assert_eq!(cs.get_property("position"), "absolute");
        let cs = cs_of("<html><body><div></div></body></html>", "", |e| e.tag == "div");
        assert_eq!(cs.get_property("position"), "static");
    }

    #[test]
    fn get_property_width_height_auto_or_px() {
        let cs = cs_of("<html><body><div></div></body></html>", "", |e| e.tag == "div");
        assert_eq!(cs.get_property("width"), "auto");
        let cs = cs_of("<html><body><div style='width:100px;height:50px'></div></body></html>", "", |e| e.tag == "div");
        assert_eq!(cs.get_property("width"), "100px");
        assert_eq!(cs.get_property("height"), "50px");
    }

    #[test]
    fn get_property_margin_longhand_and_shorthand() {
        let cs = cs_of("<html><body><div style='margin:10px 20px 30px 40px'></div></body></html>", "", |e| e.tag == "div");
        assert_eq!(cs.get_property("margin-top"), "10px");
        assert_eq!(cs.get_property("margin-right"), "20px");
        assert_eq!(cs.get_property("margin-bottom"), "30px");
        assert_eq!(cs.get_property("margin-left"), "40px");
        assert_eq!(cs.get_property("margin"), "10px 20px 30px 40px");
        let cs = cs_of("<html><body><div style='margin:5px'></div></body></html>", "", |e| e.tag == "div");
        assert_eq!(cs.get_property("margin"), "5px");
    }

    #[test]
    fn get_property_opacity_and_padding() {
        let cs = cs_of("<html><body><div style='opacity:0.5;padding:8px'></div></body></html>", "", |e| e.tag == "div");
        assert_eq!(cs.get_property("opacity"), "0.5");
        assert_eq!(cs.get_property("padding"), "8px");
        let cs = cs_of("<html><body><div></div></body></html>", "", |e| e.tag == "div");
        assert_eq!(cs.get_property("opacity"), "1");
    }

    #[test]
    fn get_property_flex_container() {
        let cs = cs_of(
            "<html><body><div style='display:flex;justify-content:center;flex-direction:column'></div></body></html>",
            "",
            |e| e.tag == "div",
        );
        assert_eq!(cs.get_property("justify-content"), "center");
        assert_eq!(cs.get_property("flex-direction"), "column");
    }

    #[test]
    fn get_property_untracked_returns_empty() {
        let cs = cs_of("<html><body><div></div></body></html>", "", |e| e.tag == "div");
        assert_eq!(cs.get_property("visibility"), "");
        assert_eq!(cs.get_property("cursor"), "");
        assert_eq!(cs.get_property("--custom-var"), "");
        assert_eq!(cs.get_property("transition"), "");
    }

    #[test]
    fn get_property_is_case_insensitive() {
        let cs = cs_of("<html><body><div></div></body></html>", "", |e| e.tag == "div");
        assert_eq!(cs.get_property("DISPLAY"), "block");
        assert_eq!(cs.get_property("Font-Size"), "16px");
    }

    #[test]
    fn property_names_all_resolve_nonempty() {
        let cs = ComputedStyle::default();
        for name in cs.property_names() {
            assert!(
                !cs.get_property(name).is_empty(),
                "property `{name}` listed in property_names() resolved to empty"
            );
        }
    }

    // ------------------------------------------------------------------------------------------
    // border-collapse / presentational hints
    // ------------------------------------------------------------------------------------------

    #[test]
    fn border_collapse_property_parses() {
        let cs = cs_of(
            r#"<html><body><table style="border-collapse: collapse"></table></body></html>"#,
            "",
            |e| e.tag == "table",
        );
        assert_eq!(cs.border_collapse, BorderCollapse::Collapse);
        assert_eq!(cs.get_property("border-collapse"), "collapse");
    }

    #[test]
    fn border_collapse_inherits_to_cells() {
        let cs = cs_of(
            r#"<html><body><table style="border-collapse: collapse"><tr><td>x</td></tr></table></body></html>"#,
            "",
            |e| e.tag == "td",
        );
        assert_eq!(cs.border_collapse, BorderCollapse::Collapse);
    }

    #[test]
    fn pres_table_border_attr_borders_table_and_cells() {
        // <table border="2"> → 2px border on the table AND 1px on each cell.
        let doc = html::parse(r#"<html><body><table border="2"><tr><td>x</td></tr></table></body></html>"#);
        let map = cascade(&doc, &[]);
        let table = elem(&doc, |e| e.tag == "table");
        let td = elem(&doc, |e| e.tag == "td");
        assert_eq!(map[&table].border.top, 2.0, "table border attr → 2px");
        assert_eq!(map[&td].border.top, 1.0, "table border attr → 1px on cells");
    }

    #[test]
    fn pres_bgcolor_named_and_hex() {
        let red = cs_of(
            r#"<html><body><table><tr><td bgcolor="red">x</td></tr></table></body></html>"#,
            "",
            |e| e.tag == "td",
        );
        assert_eq!(red.background_color, Some((255, 0, 0)));
        let hex = cs_of(
            r##"<html><body><table bgcolor="#00ff00"></table></body></html>"##,
            "",
            |e| e.tag == "table",
        );
        assert_eq!(hex.background_color, Some((0, 255, 0)));
    }

    #[test]
    fn mask_shorthand_extracts_url_and_size() {
        // `mask: url(...) no-repeat center / contain` → url + Contain size, parsing past the rest.
        let cs = cs_of(
            r#"<html><body><div class="x"></div></body></html>"#,
            r#".x { mask: url("icon.svg") no-repeat center / contain }"#,
            |e| e.tag == "div",
        );
        let m = cs.mask_image.expect("mask should parse");
        assert_eq!(m.url, "icon.svg");
        assert_eq!(m.size, MaskSize::Contain);
    }

    #[test]
    fn mask_url_resolves_against_stylesheet_base_not_document() {
        // The bug: a relative `url()` in an `@import`'d sheet at `/a/b/sheet.css` must resolve
        // against THAT sheet's URL → `/a/x.svg` (stylesheet-relative), not the document.
        let doc = html::parse(r#"<html><body><div class="x"></div></body></html>"#);
        let sheet = css::parse_with_base(
            r#".x { mask: url('../x.svg') no-repeat center / contain }"#,
            "https://site.example/a/b/sheet.css",
        );
        let map = cascade(&doc, &[sheet]);
        let id = elem(&doc, |e| e.tag == "div");
        let m = map[&id].mask_image.clone().expect("mask should parse");
        assert_eq!(m.url, "https://site.example/a/x.svg");
    }

    #[test]
    fn mask_url_resolves_against_stylesheet_dir_for_sibling_subdir() {
        // Mirrors the browserscore bug: sheet at /ui/css/icons.css, url('../icons/w3c.svg')
        // → /ui/icons/w3c.svg (NOT the document-relative /icons/w3c.svg).
        let doc = html::parse(r#"<html><body><div class="x"></div></body></html>"#);
        let sheet = css::parse_with_base(
            r#".x { mask: url('../icons/w3c.svg') no-repeat center/contain }"#,
            "https://browserscore.dev/ui/css/icons.css",
        );
        let map = cascade(&doc, &[sheet]);
        let id = elem(&doc, |e| e.tag == "div");
        let m = map[&id].mask_image.clone().expect("mask should parse");
        assert_eq!(m.url, "https://browserscore.dev/ui/icons/w3c.svg");
    }

    #[test]
    fn mask_data_url_passes_through_unchanged() {
        // `data:` masks are self-contained and must never be rewritten against a base.
        let doc = html::parse(r#"<html><body><div class="x"></div></body></html>"#);
        let sheet = css::parse_with_base(
            r#".x { mask: url("data:image/svg+xml,<svg></svg>") }"#,
            "https://site.example/a/b/sheet.css",
        );
        let map = cascade(&doc, &[sheet]);
        let id = elem(&doc, |e| e.tag == "div");
        let m = map[&id].mask_image.clone().expect("mask should parse");
        assert_eq!(m.url, "data:image/svg+xml,<svg></svg>");
    }

    #[test]
    fn mask_url_without_base_is_left_relative_for_engine_fallback() {
        // No base (inline-style / base-less sheet): the cascade leaves the url relative; the engine
        // resolves it against the document URL.
        let cs = cs_of(
            r#"<html><body><div class="x"></div></body></html>"#,
            r#".x { mask: url("icon.svg") no-repeat center / contain }"#,
            |e| e.tag == "div",
        );
        assert_eq!(cs.mask_image.expect("mask").url, "icon.svg");
    }

    #[test]
    fn webkit_mask_is_an_alias() {
        let cs = cs_of(
            r#"<html><body><div class="x"></div></body></html>"#,
            r#".x { -webkit-mask: url(a.svg) center / cover }"#,
            |e| e.tag == "div",
        );
        let m = cs.mask_image.expect("-webkit-mask should parse");
        assert_eq!(m.url, "a.svg");
        assert_eq!(m.size, MaskSize::Cover);
    }

    #[test]
    fn mask_url_resolves_var() {
        // The icon url is behind a custom property (the browserscore pattern).
        let cs = cs_of(
            r#"<html><body><div class="x"></div></body></html>"#,
            r#".x { --icon: url(glyph.svg); mask: var(--icon) no-repeat center / contain }"#,
            |e| e.tag == "div",
        );
        let m = cs.mask_image.expect("var()-indirected mask should resolve");
        assert_eq!(m.url, "glyph.svg");
    }

    #[test]
    fn mask_data_url_preserved() {
        let cs = cs_of(
            r#"<html><body><div class="x"></div></body></html>"#,
            r#".x { mask: url("data:image/svg+xml,<svg></svg>") }"#,
            |e| e.tag == "div",
        );
        let m = cs.mask_image.expect("data: mask should parse");
        assert!(m.url.starts_with("data:image/svg+xml,"));
        assert_eq!(m.size, MaskSize::Stretch, "no size keyword → Stretch (fit-to-box)");
    }

    #[test]
    fn mask_none_clears() {
        let cs = cs_of(
            r#"<html><body><div class="x"></div></body></html>"#,
            r#".x { mask: url(a.svg); mask: none }"#,
            |e| e.tag == "div",
        );
        assert!(cs.mask_image.is_none(), "mask: none clears the mask");
    }

    #[test]
    fn pres_align_center_on_cell() {
        let cs = cs_of(
            r#"<html><body><table><tr><td align="center">x</td></tr></table></body></html>"#,
            "",
            |e| e.tag == "td",
        );
        assert_eq!(cs.text_align, TextAlign::Center);
    }

    #[test]
    fn pres_cellpadding_and_cellspacing() {
        let td = cs_of(
            r#"<html><body><table cellpadding="10" cellspacing="4"><tr><td>x</td></tr></table></body></html>"#,
            "",
            |e| e.tag == "td",
        );
        assert_eq!(td.padding.top, 10.0, "cellpadding → cell padding");
        let table = cs_of(
            r#"<html><body><table cellpadding="10" cellspacing="4"><tr><td>x</td></tr></table></body></html>"#,
            "",
            |e| e.tag == "table",
        );
        assert_eq!(table.border_spacing, 4.0, "cellspacing → border-spacing");
    }

    #[test]
    fn pres_width_attr_on_cell() {
        let cs = cs_of(
            r#"<html><body><table><tr><td width="200">x</td></tr></table></body></html>"#,
            "",
            |e| e.tag == "td",
        );
        assert_eq!(cs.width, Some(200.0));
    }

    #[test]
    fn author_css_overrides_presentational_hint() {
        // bgcolor="red" but author CSS sets blue → CSS wins (hints are lowest precedence).
        let cs = cs_of(
            r#"<html><body><table><tr><td bgcolor="red">x</td></tr></table></body></html>"#,
            "td { background-color: blue }",
            |e| e.tag == "td",
        );
        assert_eq!(cs.background_color, Some((0, 0, 255)), "author CSS should beat bgcolor attr");
    }

    // ------------------------------------------------------------------------------------------
    // CSSOM resolved insets / !important / value retention
    // ------------------------------------------------------------------------------------------

    #[test]
    fn static_inset_resolves_to_computed_value() {
        // `position: static`: the inset *resolved value* is the computed value — `auto` stays `auto`,
        // percentages stay percentages, lengths absolutize to px.
        let cs = cs_of(
            "<html><body><div></div></body></html>",
            "div { position: static; top: auto; left: 10%; bottom: 1em; font-size: 10px }",
            |e| e.tag == "div",
        );
        assert_eq!(cs.resolved_inset(EdgeSide::Top, false, f32::NAN), "auto");
        assert_eq!(cs.resolved_inset(EdgeSide::Left, false, f32::NAN), "10%");
        assert_eq!(cs.resolved_inset(EdgeSide::Bottom, false, f32::NAN), "10px"); // 1em @ 10px
    }

    #[test]
    fn relative_inset_resolves_percentage_and_auto_pair() {
        let cs = cs_of(
            "<html><body><div></div></body></html>",
            "div { position: relative; top: 10%; bottom: auto }",
            |e| e.tag == "div",
        );
        // 10% of a 100px containing block → 10px; the auto bottom mirrors the negated top.
        assert_eq!(cs.resolved_inset(EdgeSide::Top, false, 100.0), "10px");
        assert_eq!(cs.resolved_inset(EdgeSide::Bottom, false, 100.0), "-10px");
    }

    #[test]
    fn nobox_inset_preserves_computed_value() {
        let cs = cs_of(
            "<html><body><div></div></body></html>",
            "div { position: absolute; left: 25% }",
            |e| e.tag == "div",
        );
        // Box-less (display:none): even an absolutely-positioned element reports the computed value.
        assert_eq!(cs.resolved_inset(EdgeSide::Left, true, 400.0), "25%");
    }

    #[test]
    fn important_declaration_wins_over_higher_specificity() {
        // `div` (low specificity) with `!important` beats `.x` (higher specificity) without it.
        let cs = cs_of(
            r#"<html><body><div class="x"></div></body></html>"#,
            "div { color: blue !important } .x { color: red }",
            |e| e.tag == "div",
        );
        assert_eq!(cs.color, (0, 0, 255), "!important should win the cascade");
        // And the value parses despite the trailing `!important`.
        assert_eq!(cs.get_property("color"), "rgb(0, 0, 255)");
    }

    #[test]
    fn split_importance_strips_keyword() {
        assert_eq!(split_importance("red !important"), ("red", true));
        assert_eq!(split_importance("rgb(0, 0, 255)!important"), ("rgb(0, 0, 255)", true));
        assert_eq!(split_importance("10px"), ("10px", false));
    }

    #[test]
    fn parse_inset_value_retains_percent_and_calc() {
        assert_eq!(parse_inset_value("auto", 16.0), InsetValue::Auto);
        assert_eq!(parse_inset_value("10%", 16.0), InsetValue::Percent(10.0));
        assert_eq!(parse_inset_value("1em", 10.0), InsetValue::Length(10.0));
        assert_eq!(parse_inset_value("calc(10% - 1px)", 16.0), InsetValue::Calc { pct: 10.0, px: -1.0 });
    }
}

