use crate::*;

/// Which side(s) of a box a value targets.
#[derive(Clone, Copy, PartialEq)]
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
pub(crate) fn eval_length(value: &str, font_size_px: f32) -> Option<f32> {
    let lower = value.trim().to_ascii_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut p = MathParser {
        chars: &chars,
        pos: 0,
        font_size: font_size_px,
    };
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
pub(crate) struct MathParser<'a> {
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
            "rem" => Some(num * crate::cascade::root_em()),
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
pub(crate) fn has_math_func(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("calc(")
        || lower.contains("min(")
        || lower.contains("max(")
        || lower.contains("clamp(")
}

/// Parse a single `list-style-type` keyword into a [`ListStyleType`] (None for unknown tokens).
pub(crate) fn parse_list_style_type(val: &str) -> Option<ListStyleType> {
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
pub(crate) fn parse_length(val: &str) -> Option<f32> {
    parse_length_fs(val, 16.0)
}

/// Like [`parse_length`] but resolves `em` against the supplied element `font_size` (CSS px). The
/// non-em paths are identical. Used for box-model edges (margin/padding), where the UA sheet uses
/// `em` values that must scale with each element's font size (e.g. `h1 { margin: 0.67em 0 }`).
pub(crate) fn parse_length_fs(val: &str, font_size: f32) -> Option<f32> {
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
    let num = |suffix: &str| {
        v.strip_suffix(suffix)
            .and_then(|n| n.trim().parse::<f32>().ok())
    };
    if let Some(px) = num("px") {
        Some(px)
    } else if let Some(pt) = num("pt") {
        Some(pt * 4.0 / 3.0)
    } else if let Some(rem) = num("rem") {
        Some(rem * crate::cascade::root_em())
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
pub(crate) fn split_importance(val: &str) -> (&str, bool) {
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
pub(crate) fn parse_inset_value(val: &str, font_size: f32) -> InsetValue {
    let v = val.trim().to_ascii_lowercase();
    if v.is_empty() || v == "auto" {
        return InsetValue::Auto;
    }
    // Plain percentage: `10%`.
    if let Some(p) = v
        .strip_suffix('%')
        .and_then(|n| n.trim().parse::<f32>().ok())
    {
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
pub(crate) fn parse_calc_percent(val: &str, font_size: f32) -> Option<InsetValue> {
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
pub(crate) fn parse_edge_length(val: &str, font_size: f32) -> Option<f32> {
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
pub(crate) fn set_edge(edges: &mut Edges, side: EdgeSide, val: &str, font_size: f32) {
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

/// Set one margin side, tracking `auto` (which resolves to 0 in the f32 — the layout resolves it).
pub(crate) fn set_margin_side(style: &mut ComputedStyle, side: EdgeSide, idx: usize, val: &str) {
    if val.trim().eq_ignore_ascii_case("auto") {
        style.margin_auto[idx] = true;
        set_edge(&mut style.margin, side, "0", style.font_size);
    } else {
        style.margin_auto[idx] = false;
        set_edge(&mut style.margin, side, val, style.font_size);
    }
}

/// Parse the `margin` shorthand (1–4 values), returning the px [`Edges`] (`auto` → 0) and a per-side
/// `[top, right, bottom, left]` flag marking which were `auto`.
pub(crate) fn parse_margin_shorthand(val: &str, font_size: f32) -> (Edges, [bool; 4]) {
    let one = |t: &str| -> (f32, bool) {
        if t.eq_ignore_ascii_case("auto") {
            (0.0, true)
        } else {
            (parse_edge_length(t, font_size).unwrap_or(0.0), false)
        }
    };
    let v: Vec<(f32, bool)> = val.split_whitespace().map(one).collect();
    let (t, r, b, l) = match v.len() {
        0 => return (Edges::default(), [false; 4]),
        1 => (v[0], v[0], v[0], v[0]),
        2 => (v[0], v[1], v[0], v[1]),
        3 => (v[0], v[1], v[2], v[1]),
        _ => (v[0], v[1], v[2], v[3]),
    };
    (
        Edges {
            top: t.0,
            right: r.0,
            bottom: b.0,
            left: l.0,
        },
        [t.1, r.1, b.1, l.1],
    )
}

/// Parse a `margin`/`padding`/`border-width` shorthand of 1–4 values.
/// CSS order: `all` / `vert horiz` / `top horiz bottom` / `top right bottom left`.
/// Returns `None` if no token parsed (leaves the existing value untouched).
pub(crate) fn parse_edges_shorthand(val: &str, font_size: f32) -> Option<Edges> {
    let parts: Vec<f32> = val
        .split_whitespace()
        .map(|t| parse_edge_length(t, font_size).unwrap_or(0.0))
        .collect();
    match parts.len() {
        1 => Some(Edges::all(parts[0])),
        2 => Some(Edges {
            top: parts[0],
            bottom: parts[0],
            right: parts[1],
            left: parts[1],
        }),
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
pub(crate) fn apply_border_shorthand(
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
    let w = if saw_none && width.is_none() {
        Some(0.0)
    } else {
        width
    };
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
pub(crate) fn parse_font_weight(val: &str) -> Option<bool> {
    let v = val.trim().to_ascii_lowercase();
    match v.as_str() {
        "bold" | "bolder" => Some(true),
        "normal" | "lighter" => Some(false),
        other => other.parse::<u32>().ok().map(|n| n >= 600),
    }
}

/// Parse a `font-size`: `Npx`, `Npt` (×4/3), or `Nem` (relative to `parent_px`). Bare numbers
/// are treated as px.
pub(crate) fn parse_font_size(val: &str, parent_px: f32) -> Option<f32> {
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
    let num = |suffix: &str| {
        v.strip_suffix(suffix)
            .and_then(|n| n.trim().parse::<f32>().ok())
    };
    if let Some(px) = num("px") {
        Some(px)
    } else if let Some(pt) = num("pt") {
        Some(pt * 4.0 / 3.0)
    } else if let Some(em) = num("em") {
        Some(em * parent_px)
    } else if let Some(rem) = num("rem") {
        Some(rem * crate::cascade::root_em())
    } else if let Some(pct) = num("%") {
        // Percentage font-size is relative to the PARENT's computed font size (e.g. `500%` on the
        // big browserscore.dev score number → 5× its parent).
        Some(pct / 100.0 * parent_px).filter(|n| *n > 0.0)
    } else {
        v.parse::<f32>().ok().filter(|n| *n > 0.0)
    }
}
