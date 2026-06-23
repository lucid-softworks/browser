use crate::*;
use std::collections::HashMap;
use std::sync::Arc;

thread_local! {
    static EMPTY_VARS: Arc<HashMap<String, String>> = Arc::new(HashMap::new());
}

/// A shared, empty custom-property environment. Returns a cheap `Arc` clone of a per-thread
/// singleton so constructing a default [`ComputedStyle`] (or a node that inherits no vars) doesn't
/// allocate a fresh map.
pub(crate) fn empty_vars() -> Arc<HashMap<String, String>> {
    EMPTY_VARS.with(Arc::clone)
}

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
        Edges {
            top: v,
            right: v,
            bottom: v,
            left: v,
        }
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

/// CSS `visibility`. Inherits. `hidden`/`collapse` keep the box in layout but hide its own content
/// (a descendant can opt back in with `visibility: visible`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Visibility {
    #[default]
    Visible,
    Hidden,
    Collapse,
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

/// CSS `float`. `none` = in normal flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Float {
    #[default]
    None,
    Left,
    Right,
}

/// CSS `clear`: which side(s) a block moves below earlier floats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Clear {
    #[default]
    None,
    Left,
    Right,
    Both,
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
        GridPlacement {
            start: None,
            end: GridEnd::Auto,
        }
    }
}

/// CSS `direction` (inline base direction). Inherited.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Direction {
    Ltr,
    Rtl,
}

/// CSS `writing-mode` (block flow direction). Inherited. We don't lay vertical modes out, but the
/// value is tracked so the CSSOM resolved value of insets (static position) maps logical→physical.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WritingMode {
    HorizontalTb,
    VerticalRl,
    VerticalLr,
}

impl WritingMode {
    /// The physical edges (block-start, inline-start) for this writing mode + `direction`.
    pub fn start_edges(self, dir: Direction) -> (EdgeSide, EdgeSide) {
        let rtl = dir == Direction::Rtl;
        match self {
            WritingMode::HorizontalTb => (
                EdgeSide::Top,
                if rtl { EdgeSide::Right } else { EdgeSide::Left },
            ),
            WritingMode::VerticalLr => (
                EdgeSide::Left,
                if rtl { EdgeSide::Bottom } else { EdgeSide::Top },
            ),
            WritingMode::VerticalRl => (
                EdgeSide::Right,
                if rtl { EdgeSide::Bottom } else { EdgeSide::Top },
            ),
        }
    }
}

/// The computed style for a single element.
#[derive(Debug, Clone, PartialEq)]
pub struct ComputedStyle {
    /// `direction` / `writing-mode` (both inherited); used for the CSSOM resolved value of insets.
    pub direction: Direction,
    pub writing_mode: WritingMode,
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
    /// CSS `visibility`. Inherited.
    pub visibility: Visibility,
    /// CSS `position`. Not inherited.
    pub position: Position,
    /// CSS `float`. Not inherited.
    pub float: Float,
    /// CSS `clear`. Not inherited.
    pub clear: Clear,
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
    /// Explicit content `width` in px (`None` = auto/percentage; see `width_pct`).
    pub width: Option<f32>,
    /// Explicit content `height` in px (`None` = auto/percentage; see `height_pct`).
    pub height: Option<f32>,
    /// `width` as a fraction (`50%` → `0.5`) when specified as a percentage; resolved against the
    /// containing block's content width in layout. `None` unless `width` is a percentage.
    pub width_pct: Option<f32>,
    /// `height` as a fraction; resolved against the containing block's (definite) content height.
    pub height_pct: Option<f32>,
    /// Whether `aspect-ratio` specifies a ratio (not just `auto`). Used for the CSSOM resolved value
    /// of `min-width`/`min-height: auto`, which stays `auto` when a box has a preferred aspect ratio.
    pub aspect_ratio_set: bool,
    /// `min-width` constraint (`None` = 0/unset). Resolved against the containing block in layout.
    pub min_width: Option<SizeConstraint>,
    /// `max-width` constraint (`None`/`none` = no maximum).
    pub max_width: Option<SizeConstraint>,
    /// `min-height` constraint (`None` = 0/unset).
    pub min_height: Option<SizeConstraint>,
    /// `max-height` constraint (`None`/`none` = no maximum).
    pub max_height: Option<SizeConstraint>,
    /// Margin thicknesses (px). Not inherited. `auto` margins resolve to 0 here; see `margin_auto`.
    pub margin: Edges,
    /// Which margins were specified as `auto` ([top, right, bottom, left]) — needed for the layout to
    /// resolve them (centering / over-constrained boxes) and for `getComputedStyle`'s used value.
    pub margin_auto: [bool; 4],
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
    /// `text-indent` in px applied to the first line of a block container (0 = none). Inherits.
    pub text_indent: f32,
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
    /// A `background-image: url(...)` source, resolved to an absolute URL against the correct base
    /// (the stylesheet base for a rule, the document base for an inline style). `None` = no image
    /// url. Reported by `getComputedStyle` as the CSSOM resolved value. Not inherited.
    pub background_image_url: Option<String>,
    /// `background-size` for the image url (gradients ignore it). Not inherited.
    pub background_size: BgSize,
    /// `background-repeat` for the image url. Not inherited.
    pub background_repeat: BgRepeat,
    /// `background-position` as (x, y) components (px/percentage; default top-left `0% 0%`). Not
    /// inherited.
    pub background_position: (BgLen, BgLen),
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
    ///
    /// Stored behind an [`Arc`] so the (often large — hundreds of entries on token-heavy sites
    /// like wikipedia.org) inherited environment is shared, not deep-cloned, across the ~99% of
    /// elements that declare no custom property of their own. The cascade only allocates a new map
    /// when an element actually changes the environment (copy-on-write).
    pub custom_props: Arc<HashMap<String, String>>,
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
pub(crate) fn parse_color_scheme(val: &str) -> Option<ColorScheme> {
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
    Linear {
        angle_deg: f32,
        stops: Vec<GradientStop>,
    },
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

/// A `background-size`/`background-position` component: a pixel length, a percentage (stored as a
/// 0..1 fraction), or `auto`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BgLen {
    Auto,
    Px(f32),
    /// Percentage as a fraction (0.5 = 50%).
    Pct(f32),
}

/// `background-size`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum BgSize {
    /// Natural image size (`auto` / unset).
    #[default]
    Auto,
    /// Scale to cover the box, preserving aspect ratio (cropped).
    Cover,
    /// Scale to fit inside the box, preserving aspect ratio (letterboxed).
    Contain,
    /// Explicit per-axis size (px / percentage / `auto`).
    Exact(BgLen, BgLen),
}

/// `background-repeat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BgRepeat {
    #[default]
    Repeat,
    RepeatX,
    RepeatY,
    NoRepeat,
}

/// A resolved `background-image: url(...)` layer for painting. Gradients use `background_gradient`;
/// this is only for raster/SVG image sources. Multiple comma-separated layers collapse to the first.
/// `position` is each axis as a fraction 0..1 (the CSS percentage convention: the image's f-point
/// aligns to the box's f-point). Out of scope: explicit length positions/sizes, multiple layers,
/// `background-attachment`, `background-origin`/`-clip` (painted in the border box).
#[derive(Debug, Clone, PartialEq)]
pub struct BgImage {
    /// The image source url (quotes stripped, `var()` resolved): a `data:` URL or a URL to fetch.
    pub url: String,
    pub size: BgSize,
    pub repeat: BgRepeat,
    /// Position as (x, y) components (px / percentage). Default `0% 0%` (top-left). Pixel offsets are
    /// what CSS sprites use (e.g. `background-position: 0 -260px`).
    pub position: (BgLen, BgLen),
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
    /// Collapse runs of spaces/tabs (like `normal`) but preserve newlines as forced breaks.
    PreLine,
}

impl WhiteSpace {
    /// Whether runs of spaces are preserved (not collapsed) under this mode.
    pub fn preserves_spaces(self) -> bool {
        matches!(self, WhiteSpace::Pre | WhiteSpace::PreWrap)
    }
    /// Whether `\n` in the source is a forced line break under this mode.
    pub fn preserves_newlines(self) -> bool {
        matches!(
            self,
            WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::PreLine
        )
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
