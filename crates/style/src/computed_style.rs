use crate::*;

impl Default for ComputedStyle {
    fn default() -> Self {
        ComputedStyle {
            // Initial text color: black on a light canvas, or a light grey when the root opted into
            // a dark `color-scheme` (resolved before the cascade — see `root_used_scheme_dark`). The
            // root inherits this, so every box gets themed default text unless author CSS overrides.
            direction: Direction::Ltr,
            writing_mode: WritingMode::HorizontalTb,
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
            float: Float::None,
            clear: Clear::None,
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
            width_pct: None,
            aspect_ratio_set: false,
            height_pct: None,
            height: None,
            min_width: None,
            max_width: None,
            min_height: None,
            max_height: None,
            margin: Edges::default(),
            margin_auto: [false; 4],
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
            text_indent: 0.0,
            white_space: WhiteSpace::Normal,
            visibility: Visibility::Visible,
            list_style_type: ListStyleType::Disc,
            underline: false,
            line_through: false,
            overline: false,
            vertical_align: VerticalAlign::Baseline,
            opacity: 1.0,
            border_radius: 0.0,
            background_gradient: None,
            background_image_url: None,
            box_shadows: Vec::new(),
            transform: None,
            transform_origin: (0.5, 0.5),
            mask_image: None,
            content: None,
            before: None,
            after: None,
            color_scheme: ColorScheme::Normal,
            custom_props: empty_vars(),
        }
    }
}

/// Format a number the way `getComputedStyle` does: an integer with no decimal point when whole
/// (`16` not `16.0`), otherwise the shortest decimal (`12.5`). Negative zero normalizes to `0`.
pub(crate) fn num(v: f32) -> String {
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
pub(crate) fn px(v: f32) -> String {
    format!("{}px", num(v))
}

/// Format a CSS-px length the same way the CSSOM resolved-value serializer does (`<n>px`, trimming
/// trailing zeros). Public so consumers (e.g. the JS `getComputedStyle` layer feeding engine-pushed
/// used values) serialize insets/margins identically to the in-crate paths.
pub fn serialize_px(v: f32) -> String {
    px(v)
}

/// Format an opaque color as `rgb(r, g, b)`.
pub(crate) fn rgb_str((r, g, b): (u8, u8, u8)) -> String {
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
    /// Map a CSS *logical* longhand (`block-size`, `margin-inline-start`, …) to the physical property
    /// it resolves to for this element's writing mode + direction. Returns `None` for non-logical
    /// names. Logical *shorthands* (`margin-block`, `padding-inline`, …) are intentionally not mapped.
    fn physical_for_logical(&self, name: &str) -> Option<String> {
        let (block_start, inline_start) = self.writing_mode.start_edges(self.direction);
        let opp = |e: EdgeSide| match e {
            EdgeSide::Top => EdgeSide::Bottom,
            EdgeSide::Bottom => EdgeSide::Top,
            EdgeSide::Left => EdgeSide::Right,
            EdgeSide::Right => EdgeSide::Left,
            EdgeSide::All => EdgeSide::All,
        };
        let side = |e: EdgeSide| match e {
            EdgeSide::Top => "top",
            EdgeSide::Right => "right",
            EdgeSide::Bottom => "bottom",
            EdgeSide::Left => "left",
            EdgeSide::All => "top",
        };
        let (bs, be, is, ie) = (
            block_start,
            opp(block_start),
            inline_start,
            opp(inline_start),
        );
        // The inline axis is horizontal in horizontal-tb, vertical otherwise.
        let inline_horiz = matches!(self.writing_mode, WritingMode::HorizontalTb);
        let size = |inline: bool| -> &'static str {
            if inline == inline_horiz {
                "width"
            } else {
                "height"
            }
        };
        Some(match name {
            "inline-size" => size(true).to_string(),
            "block-size" => size(false).to_string(),
            "min-inline-size" => format!("min-{}", size(true)),
            "min-block-size" => format!("min-{}", size(false)),
            "max-inline-size" => format!("max-{}", size(true)),
            "max-block-size" => format!("max-{}", size(false)),
            "margin-block-start" => format!("margin-{}", side(bs)),
            "margin-block-end" => format!("margin-{}", side(be)),
            "margin-inline-start" => format!("margin-{}", side(is)),
            "margin-inline-end" => format!("margin-{}", side(ie)),
            "padding-block-start" => format!("padding-{}", side(bs)),
            "padding-block-end" => format!("padding-{}", side(be)),
            "padding-inline-start" => format!("padding-{}", side(is)),
            "padding-inline-end" => format!("padding-{}", side(ie)),
            "inset-block-start" => side(bs).to_string(),
            "inset-block-end" => side(be).to_string(),
            "inset-inline-start" => side(is).to_string(),
            "inset-inline-end" => side(ie).to_string(),
            "border-block-start-width" => format!("border-{}-width", side(bs)),
            "border-block-end-width" => format!("border-{}-width", side(be)),
            "border-inline-start-width" => format!("border-{}-width", side(is)),
            "border-inline-end-width" => format!("border-{}-width", side(ie)),
            "border-block-start-style" => format!("border-{}-style", side(bs)),
            "border-block-end-style" => format!("border-{}-style", side(be)),
            "border-inline-start-style" => format!("border-{}-style", side(is)),
            "border-inline-end-style" => format!("border-{}-style", side(ie)),
            "border-block-start-color" => format!("border-{}-color", side(bs)),
            "border-block-end-color" => format!("border-{}-color", side(be)),
            "border-inline-start-color" => format!("border-{}-color", side(is)),
            "border-inline-end-color" => format!("border-{}-color", side(ie)),
            _ => return None,
        })
    }

    pub fn get_property(&self, name: &str) -> String {
        // Custom properties are case-sensitive and read straight from the resolved environment.
        let trimmed = name.trim();
        if trimmed.starts_with("--") {
            return self.custom_props.get(trimmed).cloned().unwrap_or_default();
        }
        // Normalize: lowercase + trim (callers pass kebab-case, but be defensive).
        let name = trimmed.to_ascii_lowercase();
        // Logical longhands resolve to a physical property for this element's writing mode.
        if let Some(phys) = self.physical_for_logical(&name) {
            return self.get_property(&phys);
        }
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
            "background-image" => match &self.background_image_url {
                Some(u) => format!("url(\"{u}\")"),
                // No gradient serializer yet — keep the prior empty string for gradients so this
                // doesn't regress gradient reads; report the initial `none` only when truly unset.
                None if self.background_gradient.is_some() => String::new(),
                None => "none".to_string(),
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
            // `unicode-bidi` is not modeled beyond its initial value; report the initial keyword.
            "unicode-bidi" => "normal".to_string(),
            "direction" => match self.direction {
                Direction::Ltr => "ltr".to_string(),
                Direction::Rtl => "rtl".to_string(),
            },
            "writing-mode" => match self.writing_mode {
                WritingMode::HorizontalTb => "horizontal-tb".to_string(),
                WritingMode::VerticalRl => "vertical-rl".to_string(),
                WritingMode::VerticalLr => "vertical-lr".to_string(),
            },
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
                WhiteSpace::PreLine => "pre-line",
            }
            .to_string(),
            "visibility" => match self.visibility {
                Visibility::Visible => "visible",
                Visibility::Hidden => "hidden",
                Visibility::Collapse => "collapse",
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
            "min-width" => self.min_width.map(size_constraint_str).unwrap_or_else(|| "auto".to_string()),
            "min-height" => self.min_height.map(size_constraint_str).unwrap_or_else(|| "auto".to_string()),
            "max-width" => self.max_width.map(size_constraint_str).unwrap_or_else(|| "none".to_string()),
            "max-height" => self.max_height.map(size_constraint_str).unwrap_or_else(|| "none".to_string()),

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
            // Border style isn't tracked separately; approximate from the (rendered) width: a border
            // with a non-zero width is drawn solid, otherwise the initial `none`.
            "border-top-style" => if self.border.top > 0.0 { "solid" } else { "none" }.to_string(),
            "border-right-style" => if self.border.right > 0.0 { "solid" } else { "none" }.to_string(),
            "border-bottom-style" => if self.border.bottom > 0.0 { "solid" } else { "none" }.to_string(),
            "border-left-style" => if self.border.left > 0.0 { "solid" } else { "none" }.to_string(),

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
            // Logical longhands are enumerated in computed style (logical shorthands are not).
            "inline-size",
            "block-size",
            "min-inline-size",
            "min-block-size",
            "max-inline-size",
            "max-block-size",
            "margin-block-start",
            "margin-block-end",
            "margin-inline-start",
            "margin-inline-end",
            "padding-block-start",
            "padding-block-end",
            "padding-inline-start",
            "padding-inline-end",
            "inset-block-start",
            "inset-block-end",
            "inset-inline-start",
            "inset-inline-end",
            "border-block-start-width",
            "border-block-end-width",
            "border-inline-start-width",
            "border-inline-end-width",
            "border-block-start-style",
            "border-block-end-style",
            "border-inline-start-style",
            "border-inline-end-style",
            "border-block-start-color",
            "border-block-end-color",
            "border-inline-start-color",
            "border-inline-end-color",
        ];
        // Every name here maps to a tracked field, so all are non-empty.
        NAMES.to_vec()
    }
}
