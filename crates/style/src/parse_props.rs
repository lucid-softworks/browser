use crate::*;

/// Parse a `content` value into the string a pseudo-element should render, or `None` when the
/// value generates no box. Handles: a quoted string (with minimal escape handling — `\"`, `\\`,
/// and `\XXXX`/`\XX…` hex unicode escapes); `none`/`normal` → `None`; and `attr(name)`, which is
/// returned verbatim (`attr(name)`) for [`resolve_content_attr`] to resolve once the element is
/// known. Other functional values (`counter(...)`, `url(...)`, multiple tokens, …) are simplified
/// to an empty string (a box with no text).
pub(crate) fn parse_content(val: &str) -> Option<String> {
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
pub(crate) fn unescape_content_string(s: &str) -> String {
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
pub(crate) fn resolve_content_attr(content: &str, el: &dom::ElementData) -> String {
    let lower = content.to_ascii_lowercase();
    if lower.starts_with("attr(") && content.ends_with(')') {
        let name = content[5..content.len() - 1].trim();
        return el
            .attrs
            .get(&name.to_ascii_lowercase())
            .or_else(|| {
                el.attrs
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v)
            })
            .cloned()
            .unwrap_or_default();
    }
    content.to_string()
}

/// Parse a `min-width`/`max-width`/`min-height`/`max-height` value. `none`/`auto`/empty → `None`
/// (no constraint). Supports px (and pt/unitless via [`parse_length`]) and `%`.
pub(crate) fn parse_size_constraint(val: &str) -> Option<SizeConstraint> {
    let v = val.trim().to_ascii_lowercase();
    if v.is_empty() || v == "none" || v == "auto" {
        return None;
    }
    if has_math_func(&v) {
        return eval_length(&v, 16.0).map(SizeConstraint::Px);
    }
    if let Some(p) = v.strip_suffix('%') {
        return p
            .trim()
            .parse::<f32>()
            .ok()
            .map(|x| SizeConstraint::Pct(x / 100.0));
    }
    parse_length(val).map(SizeConstraint::Px)
}

/// Parse an `inset` shorthand of 1–4 values into per-side `Option<f32>` (auto → None).
/// CSS order: `all` / `vert horiz` / `top horiz bottom` / `top right bottom left`.
pub(crate) struct OptionalEdges {
    pub(crate) top: Option<f32>,
    pub(crate) right: Option<f32>,
    pub(crate) bottom: Option<f32>,
    pub(crate) left: Option<f32>,
}

pub(crate) fn parse_optional_edges_shorthand(val: &str) -> Option<OptionalEdges> {
    let parts: Vec<Option<f32>> = val.split_whitespace().map(parse_length).collect();
    match parts.len() {
        1 => Some(OptionalEdges {
            top: parts[0],
            right: parts[0],
            bottom: parts[0],
            left: parts[0],
        }),
        2 => Some(OptionalEdges {
            top: parts[0],
            bottom: parts[0],
            right: parts[1],
            left: parts[1],
        }),
        3 => Some(OptionalEdges {
            top: parts[0],
            right: parts[1],
            left: parts[1],
            bottom: parts[2],
        }),
        n if n >= 4 => Some(OptionalEdges {
            top: parts[0],
            right: parts[1],
            bottom: parts[2],
            left: parts[3],
        }),
        _ => None,
    }
}

/// Parse a 1–2 value list into `(first, second)` of `Option<f32>` (used by inset-block/inline);
/// a single value applies to both sides.
pub(crate) fn parse_pair(val: &str) -> Option<(Option<f32>, Option<f32>)> {
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
pub(crate) fn parse_edge_pair(val: &str) -> Option<(f32, f32)> {
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
pub(crate) fn parse_line_height(val: &str, font_size: f32) -> Option<f32> {
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
pub(crate) fn apply_text_decoration(style: &mut ComputedStyle, val: &str) {
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
pub(crate) fn parse_border_radius(val: &str) -> Option<f32> {
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
pub(crate) fn split_top_commas(s: &str) -> Vec<String> {
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
pub(crate) fn parse_angle_deg(tok: &str) -> Option<f32> {
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
/// Extract the first `url(...)` source from a CSS value (surrounding quotes stripped). `None` when
/// there's no `url(...)` or the source is empty.
pub(crate) fn extract_css_url(val: &str) -> Option<String> {
    let lower = val.to_ascii_lowercase();
    let start = lower.find("url(")?;
    let rest = &val[start + 4..];
    let close = rest.find(')')?;
    let mut raw = rest[..close].trim().to_string();
    if raw.len() >= 2
        && ((raw.starts_with('"') && raw.ends_with('"'))
            || (raw.starts_with('\'') && raw.ends_with('\'')))
    {
        raw = raw[1..raw.len() - 1].to_string();
    }
    if raw.is_empty() {
        None
    } else {
        Some(raw)
    }
}

pub(crate) fn resolve_css_url(url: &str, base: Option<&str>) -> String {
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
pub(crate) fn parse_mask(val: &str) -> Option<MaskImage> {
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

/// The image-related parts pulled from a `background` shorthand value.
pub(crate) struct BgShorthand {
    pub url: Option<String>,
    pub repeat: BgRepeat,
    pub size: BgSize,
    pub position: (f32, f32),
}

/// Parse the image layer out of a `background` shorthand: `url(...)`, `repeat`, the `/ <size>` part,
/// and a `<position>`. Color and `attachment`/`origin`/`clip` tokens are ignored here (color is
/// handled separately by the caller). Best-effort — the shorthand grammar is loose.
pub(crate) fn parse_background_shorthand(val: &str) -> BgShorthand {
    let url = extract_css_url(val);
    // Drop the `url(...)` token so its contents don't pollute position/repeat scanning.
    let without_url = match (val.to_ascii_lowercase().find("url("), val.find(')')) {
        (Some(s), Some(e)) if e > s => format!("{}{}", &val[..s], &val[e + 1..]),
        _ => val.to_string(),
    };
    // `position / size`.
    let (pos_part, size_part) = match without_url.split_once('/') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => (without_url.clone(), String::new()),
    };
    let lower = pos_part.to_ascii_lowercase();
    let repeat = if lower.contains("no-repeat") {
        BgRepeat::NoRepeat
    } else if lower.contains("repeat-x") {
        BgRepeat::RepeatX
    } else if lower.contains("repeat-y") {
        BgRepeat::RepeatY
    } else {
        BgRepeat::Repeat
    };
    let size = if size_part.trim().is_empty() {
        BgSize::Auto
    } else {
        parse_bg_size(&size_part)
    };
    // Position: keep only position-ish tokens (keywords / percentages).
    let pos_str: String = pos_part
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| {
            matches!(t.as_str(), "left" | "right" | "top" | "bottom" | "center")
                || parse_percent(t).is_some()
        })
        .collect::<Vec<_>>()
        .join(" ");
    let position = parse_bg_position(&pos_str);
    BgShorthand {
        url,
        repeat,
        size,
        position,
    }
}

/// Parse `background-size`: `cover` / `contain` / `auto`, or one–two components as percentages
/// (kept as box fractions); lengths (px/em) fall back to `auto` on that axis.
pub(crate) fn parse_bg_size(val: &str) -> BgSize {
    let v = val.trim().to_ascii_lowercase();
    match v.as_str() {
        "cover" => return BgSize::Cover,
        "contain" => return BgSize::Contain,
        "" | "auto" | "auto auto" | "initial" | "unset" | "normal" => return BgSize::Auto,
        _ => {}
    }
    let comp = |t: &str| -> Option<f32> { parse_percent(t).map(|p| p / 100.0) };
    let mut it = v.split_whitespace();
    let x = it.next().and_then(comp);
    let y = it.next().and_then(comp);
    if x.is_none() && y.is_none() {
        BgSize::Auto
    } else {
        BgSize::Exact(x, y)
    }
}

/// Parse `background-repeat` (single keyword or two-value `<x> <y>`).
pub(crate) fn parse_bg_repeat(val: &str) -> BgRepeat {
    let v = val.trim().to_ascii_lowercase();
    match v.as_str() {
        "no-repeat" => return BgRepeat::NoRepeat,
        "repeat-x" => return BgRepeat::RepeatX,
        "repeat-y" => return BgRepeat::RepeatY,
        "repeat" | "" | "repeat repeat" => return BgRepeat::Repeat,
        _ => {}
    }
    let mut it = v.split_whitespace();
    match (it.next(), it.next()) {
        (Some("repeat"), Some("no-repeat")) => BgRepeat::RepeatX,
        (Some("no-repeat"), Some("repeat")) => BgRepeat::RepeatY,
        (Some("no-repeat"), Some("no-repeat")) => BgRepeat::NoRepeat,
        _ => BgRepeat::Repeat,
    }
}

/// Parse `background-position` into (x, y) fractions in 0..1. Supports keywords (left/center/right,
/// top/center/bottom) and percentages; lengths default to 0. A single value sets that axis and
/// centers the other.
pub(crate) fn parse_bg_position(val: &str) -> (f32, f32) {
    let v = val.trim().to_ascii_lowercase();
    if v.is_empty() {
        return (0.0, 0.0);
    }
    let frac_x = |t: &str| -> Option<f32> {
        match t {
            "left" => Some(0.0),
            "center" => Some(0.5),
            "right" => Some(1.0),
            _ => parse_percent(t).map(|p| p / 100.0),
        }
    };
    let frac_y = |t: &str| -> Option<f32> {
        match t {
            "top" => Some(0.0),
            "center" => Some(0.5),
            "bottom" => Some(1.0),
            _ => parse_percent(t).map(|p| p / 100.0),
        }
    };
    let toks: Vec<&str> = v.split_whitespace().collect();
    match toks.as_slice() {
        [a] => {
            // A vertical keyword alone centers x; anything else sets x and centers y.
            if *a == "top" || *a == "bottom" {
                (0.5, frac_y(a).unwrap_or(0.0))
            } else {
                (frac_x(a).unwrap_or(0.0), 0.5)
            }
        }
        [a, b, ..] => {
            // Allow keyword order swap (`top left`); otherwise treat as x then y.
            if matches!(*a, "top" | "bottom") || matches!(*b, "left" | "right") {
                (frac_x(b).unwrap_or(0.5), frac_y(a).unwrap_or(0.5))
            } else {
                (frac_x(a).unwrap_or(0.0), frac_y(b).unwrap_or(0.0))
            }
        }
        _ => (0.0, 0.0),
    }
}

/// Parse a `linear-gradient(...)` / `radial-gradient(...)` (incl. `repeating-*`) value into a
/// [`Gradient`], or `None` if the value isn't a recognized gradient. Color stops without an
/// explicit position are distributed evenly between their neighbors (0..1). Stop positions
/// expressed as `%` resolve directly; `px` lengths are resolved as a fraction of an assumed
/// 200px gradient line (best-effort, since the real line length isn't known until paint).
pub(crate) fn parse_gradient(
    val: &str,
    current: (u8, u8, u8),
    inherited: (u8, u8, u8),
) -> Option<Gradient> {
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
pub(crate) fn strip_func<'a>(lower: &str, v: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name}(");
    if lower.starts_with(&prefix) && v.ends_with(')') {
        Some(&v[prefix.len()..v.len() - 1])
    } else {
        None
    }
}

/// Parse a linear-gradient direction (`to right`, `to top left`, or an angle) into degrees in the
/// CSS convention (0=to top, 90=to right, 180=to bottom, 270=to left).
pub(crate) fn parse_linear_direction(s: &str) -> Option<f32> {
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
pub(crate) fn split_stop(part: &str) -> (&str, Option<f32>) {
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
pub(crate) fn parse_box_shadows(
    val: &str,
    current: (u8, u8, u8),
    inherited: (u8, u8, u8),
) -> Vec<BoxShadow> {
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
            color: color.unwrap_or(Rgba {
                r: current.0,
                g: current.1,
                b: current.2,
                a: 255,
            }),
        });
    }
    out
}

/// Tokenize a value on whitespace, keeping balanced parens together (so functional colors with
/// internal spaces/commas survive as one token).
pub(crate) fn tokenize_paren_aware(s: &str) -> Vec<String> {
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
pub(crate) fn parse_transform(val: &str) -> Option<[f32; 6]> {
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
            "translatex" => [
                1.0,
                0.0,
                0.0,
                1.0,
                nums.first().copied().unwrap_or(0.0),
                0.0,
            ],
            "translatey" => [
                1.0,
                0.0,
                0.0,
                1.0,
                0.0,
                nums.first().copied().unwrap_or(0.0),
            ],
            "scale" => {
                let sx = nums.first().copied().unwrap_or(1.0);
                let sy = nums.get(1).copied().unwrap_or(sx);
                [sx, 0.0, 0.0, sy, 0.0, 0.0]
            }
            "scalex" => [
                nums.first().copied().unwrap_or(1.0),
                0.0,
                0.0,
                1.0,
                0.0,
                0.0,
            ],
            "scaley" => [
                1.0,
                0.0,
                0.0,
                nums.first().copied().unwrap_or(1.0),
                0.0,
                0.0,
            ],
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
    if any {
        Some(m)
    } else {
        None
    }
}

/// The 2D-affine identity.
pub(crate) const IDENTITY: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// Multiply two affines `a` then `b` applied as `a * b` (apply `b` first, then `a`), matching CSS
/// left-to-right function composition (first listed transform is outermost).
pub(crate) fn mat_mul(a: [f32; 6], b: [f32; 6]) -> [f32; 6] {
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
pub(crate) fn transform_arg(a: &str) -> Option<f32> {
    let t = a.trim();
    if t.ends_with('%') {
        return Some(0.0); // percentage translate resolved at paint time (approx: ignore here)
    }
    parse_length(t).or_else(|| t.parse::<f32>().ok())
}

/// Split a transform value into `(function_name_lowercased, args_string)` pairs.
pub(crate) fn transform_functions(s: &str) -> Vec<(String, String)> {
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
        let name: String = chars[name_start..i]
            .iter()
            .collect::<String>()
            .trim()
            .to_ascii_lowercase();
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
pub(crate) fn parse_transform_origin(val: &str) -> (f32, f32) {
    let toks: Vec<String> = val
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .collect();
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
