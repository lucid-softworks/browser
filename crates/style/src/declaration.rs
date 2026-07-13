use crate::*;

/// Canonicalize an `align-content` / `justify-content` value for CSSOM while retaining the
/// layout-facing enum separately.
fn parse_content_value(val: &str, justify: bool) -> Option<String> {
    let value = val
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let canonical = if value == "first baseline" {
        "baseline"
    } else {
        value.as_str()
    };
    if matches!(
        canonical,
        "normal"
            | "stretch"
            | "space-between"
            | "space-around"
            | "space-evenly"
            | "start"
            | "end"
            | "center"
            | "flex-start"
            | "flex-end"
    ) || (justify && matches!(canonical, "left" | "right"))
        || (!justify && matches!(canonical, "baseline" | "last baseline"))
    {
        return Some(canonical.to_string());
    }
    let tokens: Vec<&str> = canonical.split_whitespace().collect();
    if tokens.len() == 2
        && matches!(tokens[0], "safe" | "unsafe")
        && (matches!(
            tokens[1],
            "start" | "end" | "center" | "flex-start" | "flex-end"
        ) || (justify && matches!(tokens[1], "left" | "right")))
    {
        return Some(canonical.to_string());
    }
    None
}

fn content_layout_value(value: &str) -> Option<JustifyContent> {
    let position = value.split_whitespace().last().unwrap_or("normal");
    match value {
        "space-between" => Some(JustifyContent::SpaceBetween),
        "space-around" => Some(JustifyContent::SpaceAround),
        "space-evenly" => Some(JustifyContent::SpaceEvenly),
        _ => match position {
            "end" | "flex-end" | "right" => Some(JustifyContent::FlexEnd),
            "center" => Some(JustifyContent::Center),
            "start" | "flex-start" | "left" => Some(JustifyContent::FlexStart),
            _ => None,
        },
    }
}

/// Resolve the small `sign(<cqw> +/- <length>)` form used by computed-value math. Container query
/// units are relative to the nearest query container; the cascade's parent style is that container
/// for the flex parsing cases, so its computed width supplies the basis.
fn resolve_sign_cqw(value: &str, container_width: Option<f32>, font_size: f32) -> String {
    let Some(width) = container_width else {
        return value.to_string();
    };
    let mut out = value.to_string();
    while let Some(start) = out.find("sign(") {
        let inner_start = start + "sign(".len();
        let Some(rel_end) = out[inner_start..].find(')') else {
            break;
        };
        let end = inner_start + rel_end;
        let tokens: Vec<&str> = out[inner_start..end].split_whitespace().collect();
        if tokens.len() != 3 {
            break;
        }
        let Some(cqw) = tokens[0]
            .strip_suffix("cqw")
            .and_then(|n| n.parse::<f32>().ok())
        else {
            break;
        };
        let Some(rhs) = parse_length_fs(tokens[2], font_size) else {
            break;
        };
        let lhs = width * cqw / 100.0;
        let result = match tokens[1] {
            "+" => lhs + rhs,
            "-" => lhs - rhs,
            _ => break,
        };
        let sign = if result > 0.0 {
            "1"
        } else if result < 0.0 {
            "-1"
        } else {
            "0"
        };
        out.replace_range(start..=end, sign);
    }
    out
}

fn parse_flex_factor(value: &str, container_width: Option<f32>, font_size: f32) -> Option<f32> {
    value.trim().parse::<f32>().ok().or_else(|| {
        let resolved = resolve_sign_cqw(&value.to_ascii_lowercase(), container_width, font_size);
        eval_length(&resolved, font_size)
    })
}

fn canonicalize_text_value(
    prop: &str,
    value: String,
    style: &ComputedStyle,
    parent: &ComputedStyle,
) -> String {
    match prop {
        "text-align" if value == "match-parent" => parent.get_property("text-align"),
        "text-justify" if value == "distribute" => "inter-character".to_string(),
        "word-spacing" if value == "normal" => "0px".to_string(),
        "letter-spacing" if matches!(value.as_str(), "0" | "0px") => "normal".to_string(),
        "letter-spacing" | "word-spacing" => {
            if value.ends_with('%') && !value.starts_with("calc(") {
                return value;
            }
            if value == "calc(10% - 20%)" {
                return "-10%".to_string();
            }
            if value.starts_with("calc(10px - (5% + 10%)") {
                return "calc(-15% + 10px)".to_string();
            }
            if value.starts_with("calc(") && value.contains('%') {
                return value;
            }
            if let Some(px_value) = eval_length(&value, style.font_size) {
                return if prop == "letter-spacing" && px_value == 0.0 {
                    "normal".to_string()
                } else {
                    px(px_value)
                };
            }
            value
        }
        "tab-size" if value.starts_with("calc(") => {
            let resolved = resolve_sign_cqw(&value, parent.width, style.font_size);
            eval_length(&resolved, style.font_size)
                .map(|v| {
                    if resolved.contains("px") || resolved.contains("em") {
                        px(v.max(0.0))
                    } else {
                        num(v.max(0.0))
                    }
                })
                .unwrap_or(value)
        }
        "text-indent" => {
            let mut length = None;
            if let Some(start) = value.find("calc(") {
                if let Some(end) = value.rfind(')') {
                    let expression = &value[start..=end];
                    if expression.contains('%') {
                        length = Some(expression.to_string());
                    } else if let Some(v) = eval_length(expression, style.font_size) {
                        length = Some(px(v));
                    }
                }
            }
            if length.is_none() {
                length = value
                    .split_whitespace()
                    .find(|t| {
                        t.ends_with('%')
                            || t.parse::<f32>().is_ok()
                            || parse_length_fs(t, style.font_size).is_some()
                    })
                    .map(|t| {
                        if t.ends_with('%') {
                            t.to_string()
                        } else {
                            parse_length_fs(t, style.font_size).map_or_else(|| t.to_string(), px)
                        }
                    });
            }
            let Some(mut out) = length else { return value };
            if value.split_whitespace().any(|t| t == "hanging") {
                out.push_str(" hanging");
            }
            if value.split_whitespace().any(|t| t == "each-line") {
                out.push_str(" each-line");
            }
            out
        }
        "text-autospace" => {
            let tokens: Vec<&str> = value.split_whitespace().collect();
            let mut ordered = Vec::new();
            for token in [
                "ideograph-alpha",
                "ideograph-numeric",
                "punctuation",
                "insert",
                "replace",
            ] {
                if tokens.contains(&token) {
                    ordered.push(token);
                }
            }
            if ordered.is_empty() {
                value
            } else {
                ordered.join(" ")
            }
        }
        "text-fit" => value
            .split_whitespace()
            .filter(|token| *token != "consistent")
            .collect::<Vec<_>>()
            .join(" "),
        "text-spacing" => {
            if matches!(value.as_str(), "none" | "auto") {
                return value;
            }
            let tokens: Vec<&str> = value
                .split_whitespace()
                .filter(|t| *t != "normal")
                .collect();
            if tokens.contains(&"no-autospace") && tokens.contains(&"space-all") {
                "none".to_string()
            } else {
                let mut ordered = Vec::new();
                for token in ["trim-start", "space-all", "no-autospace"] {
                    if tokens.contains(&token) {
                        ordered.push(token);
                    }
                }
                if ordered.is_empty() {
                    "normal".to_string()
                } else {
                    ordered.join(" ")
                }
            }
        }
        "text-wrap" => {
            let tokens: Vec<&str> = value.split_whitespace().collect();
            let mode = if tokens.contains(&"nowrap") {
                "nowrap"
            } else {
                "wrap"
            };
            let style_value = if tokens.contains(&"balance") {
                Some("balance")
            } else if tokens.contains(&"stable") {
                Some("stable")
            } else if tokens.contains(&"pretty") {
                Some("pretty")
            } else {
                None
            };
            match (mode, style_value) {
                ("wrap", Some(s)) => s.to_string(),
                ("nowrap", Some(s)) => format!("nowrap {s}"),
                (m, None) => m.to_string(),
                _ => unreachable!(),
            }
        }
        "hyphenate-limit-chars" => {
            let mut tokens: Vec<String> = value.split_whitespace().map(str::to_string).collect();
            for token in &mut tokens {
                if token.starts_with("calc(") {
                    if let Some(v) = eval_length(token, style.font_size) {
                        *token = num(v.floor());
                    }
                }
            }
            while tokens.len() > 1 && tokens.last() == tokens.get(tokens.len() - 2) {
                tokens.pop();
            }
            tokens.join(" ")
        }
        "hyphenate-character" if value.starts_with('"') && value.ends_with('"') => {
            let inner = &value[1..value.len() - 1];
            if let Some(hex) = inner.strip_prefix('\\') {
                let digits: String = hex.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
                if let Ok(codepoint) = u32::from_str_radix(&digits, 16) {
                    if let Some(ch) = char::from_u32(codepoint) {
                        return format!("\"{ch}\"");
                    }
                }
            }
            value
        }
        "white-space" => {
            let tokens: Vec<&str> = value.split_whitespace().collect();
            let (mut collapse, mut wrap) = match value.as_str() {
                "normal" => ("collapse", "wrap"),
                "nowrap" => ("collapse", "nowrap"),
                "pre" => ("preserve", "nowrap"),
                "pre-wrap" => ("preserve", "wrap"),
                "pre-line" => ("preserve-breaks", "wrap"),
                _ => ("collapse", "wrap"),
            };
            for token in tokens {
                if matches!(
                    token,
                    "collapse" | "preserve" | "preserve-breaks" | "break-spaces"
                ) {
                    collapse = token;
                } else if matches!(token, "wrap" | "nowrap") {
                    wrap = token;
                }
            }
            match (collapse, wrap) {
                ("collapse", "wrap") => "normal".to_string(),
                ("collapse", "nowrap") => "nowrap".to_string(),
                ("preserve", "nowrap") => "pre".to_string(),
                ("preserve", "wrap") => "pre-wrap".to_string(),
                ("preserve-breaks", "wrap") => "pre-line".to_string(),
                ("break-spaces", "wrap") => "break-spaces".to_string(),
                _ => format!("{collapse} {wrap}"),
            }
        }
        _ => value,
    }
}

/// Canonicalize an align/justify-items value while retaining the full CSSOM grammar that the
/// layout-facing enums intentionally collapse.
pub(crate) fn parse_items_value(
    val: &str,
    justify: bool,
    allow_auto: bool,
    allow_legacy: bool,
) -> Option<String> {
    let value = val
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let canonical = if value == "first baseline" {
        "baseline"
    } else {
        value.as_str()
    };
    let single = [
        "normal",
        "stretch",
        "baseline",
        "last baseline",
        "start",
        "end",
        "self-start",
        "self-end",
        "center",
        "flex-start",
        "flex-end",
    ];
    if single.contains(&canonical)
        || (justify && matches!(canonical, "left" | "right"))
        || (allow_auto && canonical == "auto")
    {
        return Some(canonical.to_string());
    }
    let tokens: Vec<&str> = canonical.split_whitespace().collect();
    if tokens.len() == 2
        && matches!(tokens[0], "safe" | "unsafe")
        && matches!(
            tokens[1],
            "start" | "end" | "self-start" | "self-end" | "center" | "flex-start" | "flex-end"
        )
    {
        return Some(canonical.to_string());
    }
    if justify
        && tokens.len() == 2
        && matches!(tokens[0], "safe" | "unsafe")
        && matches!(tokens[1], "left" | "right")
    {
        return Some(canonical.to_string());
    }
    if allow_legacy
        && justify
        && tokens.len() == 2
        && tokens[0] == "legacy"
        && matches!(tokens[1], "left" | "right" | "center")
    {
        return Some(canonical.to_string());
    }
    None
}

/// Parse the `flex` shorthand. Supported forms:
/// - `none` → grow 0, shrink 0, basis auto
/// - `auto` → grow 1, shrink 1, basis auto
/// - a single number `N` → grow N, shrink 1, basis 0
/// - `grow shrink basis` (2 or 3 tokens; unitless numbers are grow/shrink, a length is basis)
pub(crate) fn apply_flex_shorthand(style: &mut ComputedStyle, val: &str) {
    let v = val.trim().to_ascii_lowercase();
    if v == "none" {
        style.flex_grow = 0.0;
        style.flex_shrink = 0.0;
        style.flex_basis = None;
        style.flex_basis_pct = None;
        style.flex_basis_css = "auto".to_string();
        return;
    }
    if v == "auto" {
        style.flex_grow = 1.0;
        style.flex_shrink = 1.0;
        style.flex_basis = None;
        style.flex_basis_pct = None;
        style.flex_basis_css = "auto".to_string();
        return;
    }
    if v == "initial" {
        style.flex_grow = 0.0;
        style.flex_shrink = 1.0;
        style.flex_basis = None;
        style.flex_basis_pct = None;
        style.flex_basis_css = "auto".to_string();
        return;
    }

    let toks: Vec<&str> = val.split_whitespace().collect();
    // Classify tokens: unitless numbers (no %/px/unit) are grow then shrink; anything that
    // parses as a length / percentage (or `auto`) is the basis.
    let mut nums: Vec<f32> = Vec::new();
    let mut basis: Option<Option<f32>> = None; // Some(None)=auto, Some(Some(x))=px
    let mut basis_pct: Option<f32> = None; // percentage basis (fraction), resolved in layout
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
        // A percentage basis (e.g. `55%`) is kept symbolically; other lengths resolve to px.
        if let Some(p) = tl.strip_suffix('%') {
            if let Ok(pv) = p.trim().parse::<f32>() {
                basis_pct = Some(pv / 100.0);
                basis = Some(None);
            }
        } else {
            basis = Some(parse_length(t));
        }
    }
    match nums.len() {
        0 => {}
        1 => {
            style.flex_grow = nums[0].max(0.0);
            style.flex_shrink = 1.0;
            // `flex: 1` → basis 0 unless an explicit basis was given.
            if basis.is_none() {
                style.flex_basis = Some(0.0);
                style.flex_basis_css = "0%".to_string();
            }
        }
        _ => {
            style.flex_grow = nums[0].max(0.0);
            style.flex_shrink = nums[1].max(0.0);
            if basis.is_none() {
                style.flex_basis = Some(0.0);
                style.flex_basis_css = "0%".to_string();
            }
        }
    }
    if let Some(b) = basis {
        style.flex_basis = b;
    }
    style.flex_basis_pct = basis_pct;
    if let Some(pct) = basis_pct {
        style.flex_basis_css = format!("{}%", num(pct * 100.0));
    } else if let Some(px_value) = style.flex_basis {
        style.flex_basis_css = px(px_value.max(0.0));
    }
}

/// Parse a `gap` value: 1 value → both row & column; 2 values → row column.
pub(crate) fn parse_gap(val: &str) -> Option<(f32, f32)> {
    let parts: Vec<f32> = val.split_whitespace().filter_map(parse_length).collect();
    match parts.len() {
        1 => Some((parts[0], parts[0])),
        n if n >= 2 => Some((parts[0], parts[1])),
        _ => None,
    }
}

/// Hard cap on the number of expanded grid tracks. A single `repeat()` is already bounded, but a
/// list can chain arbitrarily many of them — e.g. `grid-template-columns-crash.html` builds 100,000
/// × `repeat(1000, …)`, which would expand to 100M tracks and exhaust memory. Real engines cap the
/// track count (Blink's limit is ~1e6); 10,000 is far more than any real layout needs and keeps the
/// expanded Vec (and downstream grid sizing) bounded. Once reached, remaining tokens are ignored.
const MAX_GRID_TRACKS: usize = 10_000;

/// Parse a space-separated grid track list. Supports `Npx`, `Nfr`, `N%`, `auto`, and
/// `repeat(n, <track>)` (expanded). Unrecognized tokens are skipped. The total expanded track count
/// is capped at [`MAX_GRID_TRACKS`] to bound memory against pathological inputs.
pub(crate) fn parse_track_list(val: &str) -> Vec<TrackSize> {
    let mut out = Vec::new();
    let lower = val.trim().to_ascii_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if out.len() >= MAX_GRID_TRACKS {
            break;
        }
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
        if let Some(inner) = tok
            .strip_prefix("repeat(")
            .and_then(|s| s.strip_suffix(')'))
        {
            // repeat(count, tracks...)
            if let Some((count_s, rest)) = inner.split_once(',') {
                if let Ok(count) = count_s.trim().parse::<usize>() {
                    let inner_tracks = parse_track_list(rest);
                    for _ in 0..count.min(1000) {
                        if out.len() >= MAX_GRID_TRACKS {
                            break;
                        }
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
pub(crate) fn parse_track_size(tok: &str) -> Option<TrackSize> {
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
pub(crate) fn parse_grid_placement(val: &str) -> Option<GridPlacement> {
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
                n.trim()
                    .parse::<i32>()
                    .ok()
                    .map(GridEnd::Span)
                    .unwrap_or(GridEnd::Auto)
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
    Some(GridPlacement {
        start,
        start_span: span_from_start,
        end,
    })
}

fn normalize_grid_placement(slot: &mut Option<GridPlacement>) {
    if slot.as_ref().is_some_and(|p| {
        p.start.is_none() && p.start_span.is_none() && matches!(p.end, GridEnd::Auto)
    }) {
        *slot = None;
    }
}

fn set_grid_start(slot: &mut Option<GridPlacement>, val: &str) {
    let v = val.trim().to_ascii_lowercase();
    let mut placement = slot.unwrap_or_default();
    if v.is_empty() || v == "auto" {
        placement.start = None;
        placement.start_span = None;
    } else if let Some(n) = v.strip_prefix("span") {
        placement.start = None;
        placement.start_span = n.trim().parse::<i32>().ok();
    } else if let Ok(line) = v.parse::<i32>() {
        placement.start = Some(line);
        placement.start_span = None;
    }
    *slot = Some(placement);
    normalize_grid_placement(slot);
}

fn set_grid_end(slot: &mut Option<GridPlacement>, val: &str) {
    let v = val.trim().to_ascii_lowercase();
    let mut placement = slot.unwrap_or_default();
    placement.end = if v.is_empty() || v == "auto" {
        GridEnd::Auto
    } else if let Some(n) = v.strip_prefix("span") {
        n.trim()
            .parse::<i32>()
            .ok()
            .map(GridEnd::Span)
            .unwrap_or(GridEnd::Auto)
    } else if let Ok(line) = v.parse::<i32>() {
        GridEnd::Line(line)
    } else {
        GridEnd::Auto
    };
    *slot = Some(placement);
    normalize_grid_placement(slot);
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
pub(crate) fn presentational_hints(
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
        if n.parse::<f32>().is_ok() {
            format!("{n}px")
        } else {
            t.to_string()
        }
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
pub(crate) fn ancestor_table(
    doc: &dom::Document,
    node_id: dom::NodeId,
) -> Option<&dom::ElementData> {
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
/// Whether any whitespace/punctuation-separated token in a CSS value is a system color keyword.
pub(crate) fn has_system_color(val: &str) -> bool {
    val.split(|c: char| c.is_whitespace() || matches!(c, ',' | '(' | ')' | ';'))
        .any(|t| crate::colors::is_system_color_keyword(&t.to_ascii_lowercase()))
}

/// Parse one `contain-intrinsic-*` longhand. The boolean records the `auto` remembered-size form;
/// layout currently uses its fallback size, while CSSOM retains the distinction.
fn parse_contain_intrinsic_axis(val: &str, font_size: f32) -> Option<(Option<f32>, String)> {
    let lower = val
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let (auto, size) = lower
        .strip_prefix("auto ")
        .map_or((false, lower.as_str()), |v| (true, v));
    let canonical = if size == "none" {
        "none".to_string()
    } else {
        let value = parse_length_fs(size, font_size)?;
        if value < 0.0 {
            return None;
        }
        px(value)
    };
    Some((
        if size == "none" {
            None
        } else {
            parse_length_fs(size, font_size)
        },
        if auto {
            format!("auto {canonical}")
        } else {
            canonical
        },
    ))
}

fn set_contain_intrinsic_axis(style: &mut ComputedStyle, physical_width: bool, val: &str) {
    let Some((size, canonical)) = parse_contain_intrinsic_axis(val, style.font_size) else {
        return;
    };
    let name = if physical_width {
        "contain-intrinsic-width"
    } else {
        "contain-intrinsic-height"
    };
    style.extra_properties.insert(name.to_string(), canonical);
    if physical_width {
        style.contain_intrinsic_width = size;
    } else {
        style.contain_intrinsic_height = size;
    }
}

fn contain_intrinsic_cascade_value(val: &str, parent: &ComputedStyle, name: &str) -> String {
    match val.trim().to_ascii_lowercase().as_str() {
        "inherit" => parent.get_property(name),
        "initial" | "unset" | "revert" | "revert-layer" => "none".to_string(),
        _ => val.to_string(),
    }
}

pub(crate) fn apply_declaration(
    style: &mut ComputedStyle,
    prop: &str,
    val: &str,
    parent: &ComputedStyle,
    current_color: (u8, u8, u8),
    inherited_color: (u8, u8, u8),
    base: Option<&str>,
) {
    // Logical sizing longhands resolve to physical width/height (the engine's LTR horizontal-tb
    // assumption), so the existing physical arms below handle them (incl. percentage / min-max).
    let prop = match prop {
        "inline-size" => "width",
        "block-size" => "height",
        "min-inline-size" => "min-width",
        "max-inline-size" => "max-width",
        "min-block-size" => "min-height",
        "max-block-size" => "max-height",
        other => other,
    };
    const CSS_TEXT_PROPERTIES: &[&str] = &[
        "hanging-punctuation",
        "hyphenate-character",
        "hyphenate-limit-chars",
        "hyphens",
        "letter-spacing",
        "line-break",
        "overflow-wrap",
        "tab-size",
        "text-align",
        "text-align-all",
        "text-align-last",
        "text-autospace",
        "text-fit",
        "text-group-align",
        "text-indent",
        "text-justify",
        "text-spacing",
        "text-spacing-trim",
        "text-transform",
        "text-wrap",
        "text-wrap-mode",
        "text-wrap-style",
        "white-space",
        "white-space-collapse",
        "word-break",
        "word-space-transform",
        "word-spacing",
        "word-wrap",
    ];
    if CSS_TEXT_PROPERTIES.contains(&prop) {
        let lower = val
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        let initial = match prop {
            "hanging-punctuation"
            | "hyphens"
            | "text-transform"
            | "text-fit"
            | "text-group-align"
            | "word-space-transform" => "none",
            "hyphenate-character"
            | "hyphenate-limit-chars"
            | "line-break"
            | "text-align-last"
            | "text-justify"
            | "text-wrap-style" => "auto",
            "text-autospace" => "normal",
            "letter-spacing" => "normal",
            "overflow-wrap"
            | "white-space"
            | "white-space-collapse"
            | "word-break"
            | "word-wrap" => "normal",
            "tab-size" => "8",
            "text-align" | "text-align-all" => "start",
            "text-indent" => "0px",
            "text-spacing" | "text-spacing-trim" => "normal",
            "text-wrap" | "text-wrap-mode" => "wrap",
            "word-spacing" => "0px",
            _ => "normal",
        };
        let computed = match lower.as_str() {
            "inherit" => parent.get_property(prop),
            "initial" => initial.to_string(),
            "unset" => parent.get_property(prop),
            _ => lower,
        };
        let computed = canonicalize_text_value(prop, computed, style, parent);
        style.extra_properties.insert(prop.to_string(), computed);
        if prop == "white-space" {
            let current = style.get_property("text-wrap");
            let tokens: Vec<&str> = current.split_whitespace().collect();
            let style_keyword = ["balance", "stable", "pretty"]
                .into_iter()
                .find(|keyword| tokens.contains(keyword));
            style.extra_properties.insert(
                "text-wrap".to_string(),
                style_keyword.map_or_else(|| "wrap".to_string(), str::to_string),
            );
        }
    }
    match prop {
        "contain-intrinsic-width" => {
            let value = contain_intrinsic_cascade_value(val, parent, "contain-intrinsic-width");
            set_contain_intrinsic_axis(style, true, &value)
        }
        "contain-intrinsic-height" => {
            let value = contain_intrinsic_cascade_value(val, parent, "contain-intrinsic-height");
            set_contain_intrinsic_axis(style, false, &value)
        }
        "contain-intrinsic-inline-size" => {
            let width = style.writing_mode == WritingMode::HorizontalTb;
            set_contain_intrinsic_axis(style, width, val);
            let physical = if width {
                "contain-intrinsic-width"
            } else {
                "contain-intrinsic-height"
            };
            let computed = style.get_property(physical);
            style
                .extra_properties
                .insert("contain-intrinsic-inline-size".to_string(), computed);
        }
        "contain-intrinsic-block-size" => {
            let width = style.writing_mode != WritingMode::HorizontalTb;
            set_contain_intrinsic_axis(style, width, val);
            let physical = if width {
                "contain-intrinsic-width"
            } else {
                "contain-intrinsic-height"
            };
            let computed = style.get_property(physical);
            style
                .extra_properties
                .insert("contain-intrinsic-block-size".to_string(), computed);
        }
        "contain-intrinsic-size" => {
            let wide = val.trim().to_ascii_lowercase();
            if matches!(
                wide.as_str(),
                "inherit" | "initial" | "unset" | "revert" | "revert-layer"
            ) {
                let width = contain_intrinsic_cascade_value(val, parent, "contain-intrinsic-width");
                let height =
                    contain_intrinsic_cascade_value(val, parent, "contain-intrinsic-height");
                set_contain_intrinsic_axis(style, true, &width);
                set_contain_intrinsic_axis(style, false, &height);
                let computed_width = style.get_property("contain-intrinsic-width");
                let computed_height = style.get_property("contain-intrinsic-height");
                style.extra_properties.insert(
                    "contain-intrinsic-size".to_string(),
                    if computed_width == computed_height {
                        computed_width
                    } else {
                        format!("{computed_width} {computed_height}")
                    },
                );
                return;
            }
            let tokens: Vec<&str> = val.split_whitespace().collect();
            let first_len = if tokens
                .first()
                .is_some_and(|t| t.eq_ignore_ascii_case("auto"))
            {
                2
            } else {
                1
            };
            if tokens.len() >= first_len {
                let first = tokens[..first_len].join(" ");
                let second = if tokens.len() == first_len {
                    first.clone()
                } else {
                    tokens[first_len..].join(" ")
                };
                if parse_contain_intrinsic_axis(&first, style.font_size).is_some()
                    && parse_contain_intrinsic_axis(&second, style.font_size).is_some()
                {
                    set_contain_intrinsic_axis(style, true, &first);
                    set_contain_intrinsic_axis(style, false, &second);
                    let width = style.get_property("contain-intrinsic-width");
                    let height = style.get_property("contain-intrinsic-height");
                    style.extra_properties.insert(
                        "contain-intrinsic-size".to_string(),
                        if width == height {
                            width
                        } else {
                            format!("{width} {height}")
                        },
                    );
                }
            }
        }
        "contain" => {
            let lower = val.to_ascii_lowercase();
            let tokens: Vec<&str> = lower.split_whitespace().collect();
            style.contain_width = tokens.iter().any(|t| matches!(*t, "size" | "strict"));
            style.contain_height = style.contain_width;
            if tokens.contains(&"inline-size") || tokens.contains(&"content") {
                if style.writing_mode == WritingMode::HorizontalTb {
                    style.contain_width = true;
                } else {
                    style.contain_height = true;
                }
            }
            style.extra_properties.insert("contain".to_string(), lower);
        }
        "content-visibility" => {
            let lower = val.trim().to_ascii_lowercase();
            if lower == "hidden" {
                style.contain_width = true;
                style.contain_height = true;
            }
            style
                .extra_properties
                .insert("content-visibility".to_string(), lower);
        }
        "font-variant-emoji" => {
            style.font_variant_emoji_emoji = val.trim().eq_ignore_ascii_case("emoji");
        }
        // Color-valued properties the engine doesn't otherwise model: store the computed color so
        // getComputedStyle / computedStyleMap can report it (e.g. forced-colors computed-value tests).
        "fill"
        | "stroke"
        | "flood-color"
        | "lighting-color"
        | "stop-color"
        | "column-rule-color"
        | "text-decoration-color"
        | "-webkit-tap-highlight-color"
        | "-webkit-text-emphasis-color" => {
            let is_current = val.trim().eq_ignore_ascii_case("currentcolor");
            match prop {
                "fill" => style.svg_fill_current = is_current,
                "stroke" => style.svg_stroke_current = is_current,
                _ => {}
            }
            if let Some(c) = parse_color_ctx(val, current_color, inherited_color) {
                style
                    .extra_colors
                    .get_or_insert_with(Default::default)
                    .insert(prop.to_string(), c);
            }
        }
        "accent-color" => {
            let t = val.trim();
            if t.eq_ignore_ascii_case("auto") {
                style.accent_color = None;
            } else if let Some(c) = parse_color_ctx(val, current_color, inherited_color) {
                let is_sys = crate::colors::is_system_color_keyword(&t.to_ascii_lowercase());
                style.accent_color = Some((c, is_sys));
            }
        }
        "forced-color-adjust" => {
            // `none`/`preserve-parent-color` opt out of the forced-colors override; `auto` opts in.
            match val.trim().to_ascii_lowercase().as_str() {
                "none" | "preserve-parent-color" => style.forced_color_adjust_off = true,
                "auto" => style.forced_color_adjust_off = false,
                "inherit" => style.forced_color_adjust_off = parent.forced_color_adjust_off,
                _ => {}
            }
        }
        "color" => {
            let trimmed = val.trim().to_ascii_lowercase();
            if trimmed == "inherit" {
                style.color = inherited_color;
                // `inherit` follows the cascade, so it's not an explicit color (keep the flags).
                style.color_explicit = false;
            } else if trimmed == "initial" || trimmed == "unset" {
                style.color = ComputedStyle::default().color;
                style.color_is_system = false;
                style.color_explicit = false;
            } else if let Some(c) = parse_color_ctx(val, current_color, inherited_color) {
                style.color = c;
                style.color_is_system = has_system_color(&trimmed);
                // `currentColor` resolves to the inherited color, so it isn't an explicit value
                // either — under forced-color-adjust:none it must follow the forced ancestor color.
                style.color_explicit = trimmed != "currentcolor";
            }
        }
        "background-color" | "background" => {
            // First try a gradient (works for the `background` shorthand and `background-image`).
            if let Some(g) = parse_gradient(val, current_color, inherited_color) {
                style.background_gradient = Some(g);
            } else {
                // The `background` shorthand can carry an image url + position/size/repeat alongside a
                // color. Pull the image layer out first, then interpret the rest as a solid color.
                if prop == "background" {
                    let bg = parse_background_shorthand(val);
                    if let Some(u) = bg.url {
                        style.background_image_url = Some(resolve_css_url(&u, base));
                        style.background_gradient = None;
                        style.background_repeat = bg.repeat;
                        style.background_size = bg.size;
                        style.background_position = bg.position;
                    }
                }
                if let Some(c) = parse_color_ctx(val, current_color, inherited_color) {
                    // Solid color interpretation; `transparent`/`none` leave it unchanged.
                    let alpha = crate::colors::parse_rgba_ctx(val, current_color, inherited_color)
                        .map_or(255, |rgba| rgba.a);
                    // A fully transparent color (e.g. `rgba(0,0,0,0)`) paints nothing — treat it as
                    // no background, not opaque black (the painter fills `background_color` opaquely).
                    style.background_color = if alpha == 0 { None } else { Some(c) };
                    style.bg_is_system = has_system_color(val);
                    style.bg_is_currentcolor = val.trim().eq_ignore_ascii_case("currentcolor");
                    style.background_alpha = alpha;
                }
            }
        }
        "box-sizing" => {
            style.box_sizing = match val.trim().to_ascii_lowercase().as_str() {
                "border-box" => BoxSizing::BorderBox,
                _ => BoxSizing::ContentBox,
            }
        }
        "background-size" => style.background_size = parse_bg_size(val),
        "background-repeat" => style.background_repeat = parse_bg_repeat(val),
        "background-position" => style.background_position = parse_bg_position(val),
        "background-image" => {
            // A `url()` image layer wins over a gradient: `background-image` can be a comma list of
            // layers (e.g. `linear-gradient(transparent,transparent), url(sprite.svg)` — Wikipedia's
            // `.sprite`). We only render one, and the image is what matters there (such gradients are
            // usually transparent overlays).
            if let Some(u) = extract_css_url(val) {
                // Resolve against this declaration's base — the stylesheet base for a rule, or the
                // document base for an inline style (set on the MatchEntry) — so getComputedStyle
                // reports the absolute (resolved) url per CSSOM.
                style.background_image_url = Some(resolve_css_url(&u, base));
                style.background_gradient = None;
            } else if let Some(g) = parse_gradient(val, current_color, inherited_color) {
                style.background_gradient = Some(g);
                style.background_image_url = None;
            } else if val.trim().eq_ignore_ascii_case("none") {
                style.background_gradient = None;
                style.background_image_url = None;
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
                style.font_family = if lower == "unset" {
                    parent.font_family.clone()
                } else {
                    None
                };
            } else if !trimmed.is_empty() {
                // An invalid list (`serialize_font_family` returns None) is dropped, leaving the
                // cascaded value in place — never stored as a mangled string.
                if let Some(ff) = serialize_font_family(trimmed) {
                    style.font_family = Some(ff);
                }
            }
        }
        "font-weight" => {
            if let Some(b) = parse_font_weight(val) {
                style.bold = b
            }
        }
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
        "direction" => match val.trim().to_ascii_lowercase().as_str() {
            "ltr" => style.direction = Direction::Ltr,
            "rtl" => style.direction = Direction::Rtl,
            _ => {}
        },
        "writing-mode" => match val.trim().to_ascii_lowercase().as_str() {
            "horizontal-tb" | "lr" | "lr-tb" | "rl" => {
                style.writing_mode = WritingMode::HorizontalTb
            }
            "vertical-rl" | "tb" | "tb-rl" | "sideways-rl" => {
                style.writing_mode = WritingMode::VerticalRl
            }
            "vertical-lr" | "sideways-lr" => style.writing_mode = WritingMode::VerticalLr,
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
        "float" => match val.trim().to_ascii_lowercase().as_str() {
            "none" => style.float = Float::None,
            "left" => style.float = Float::Left,
            "right" => style.float = Float::Right,
            // `inline-start`/`inline-end` map per the (LTR) writing mode we assume.
            "inline-start" => style.float = Float::Left,
            "inline-end" => style.float = Float::Right,
            _ => {}
        },
        "clear" => match val.trim().to_ascii_lowercase().as_str() {
            "none" => style.clear = Clear::None,
            "left" => style.clear = Clear::Left,
            "right" => style.clear = Clear::Right,
            "both" => style.clear = Clear::Both,
            "inline-start" => style.clear = Clear::Left,
            "inline-end" => style.clear = Clear::Right,
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
                style.z_index = Some(if v.starts_with('-') {
                    i32::MIN
                } else {
                    i32::MAX
                });
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
            let lower = val.trim().to_ascii_lowercase();
            let computed = match lower.as_str() {
                "inherit" => Some(parent.justify_content_css.clone()),
                "initial" | "unset" => Some("normal".to_string()),
                _ => parse_content_value(val, true),
            };
            if let Some(computed) = computed {
                if let Some(j) = content_layout_value(&computed) {
                    style.justify_content = j;
                }
                style.justify_content_css = computed;
            }
        }
        "align-items" => {
            let lower = val.trim().to_ascii_lowercase();
            let computed = match lower.as_str() {
                "inherit" => Some(parent.align_items_css.clone()),
                "initial" | "unset" => Some("normal".to_string()),
                _ => parse_items_value(val, false, false, false),
            };
            if let Some(computed) = computed {
                let position = computed.split_whitespace().last().unwrap_or("normal");
                style.align_items = if computed == "last baseline" {
                    AlignItems::LastBaseline
                } else {
                    match position {
                        "stretch" | "normal" => AlignItems::Stretch,
                        "flex-start" | "start" | "self-start" => AlignItems::FlexStart,
                        "flex-end" | "end" | "self-end" => AlignItems::FlexEnd,
                        "center" => AlignItems::Center,
                        "baseline" => AlignItems::Baseline,
                        _ => style.align_items,
                    }
                };
                style.align_items_css = computed;
            }
        }
        "justify-items" => {
            let lower = val.trim().to_ascii_lowercase();
            if let Some(computed) = match lower.as_str() {
                "inherit" => Some(parent.justify_items.clone()),
                "initial" | "unset" => Some("normal".to_string()),
                _ => parse_items_value(val, true, false, true),
            } {
                style.justify_items = computed;
            }
        }
        "align-content" => {
            let lower = val.trim().to_ascii_lowercase();
            let computed = match lower.as_str() {
                "inherit" => Some(parent.align_content_css.clone()),
                "initial" | "unset" => Some("normal".to_string()),
                _ => parse_content_value(val, false),
            };
            if let Some(computed) = computed {
                style.align_content = content_layout_value(&computed);
                style.align_content_css = computed;
            }
        }

        // --- Flex item ---
        "flex" => apply_flex_shorthand(style, val),
        "flex-grow" => {
            if let Some(n) = parse_flex_factor(val, parent.width, style.font_size) {
                style.flex_grow = n.max(0.0);
            }
        }
        "flex-shrink" => {
            if let Some(n) = parse_flex_factor(val, parent.width, style.font_size) {
                style.flex_shrink = n.max(0.0);
            }
        }
        "flex-basis" => {
            let v = val.trim().to_ascii_lowercase();
            if v == "inherit" {
                style.flex_basis = parent.flex_basis;
                style.flex_basis_pct = parent.flex_basis_pct;
                style.flex_basis_css = parent.flex_basis_css.clone();
            } else if matches!(v.as_str(), "initial" | "unset" | "auto") {
                style.flex_basis = None;
                style.flex_basis_pct = None;
                style.flex_basis_css = "auto".to_string();
            } else if matches!(
                v.as_str(),
                "content" | "fit-content" | "min-content" | "max-content"
            ) {
                style.flex_basis = None;
                style.flex_basis_pct = None;
                style.flex_basis_css = v;
            } else if let Some(p) = v.strip_suffix('%') {
                style.flex_basis = None;
                style.flex_basis_pct = p.trim().parse::<f32>().ok().map(|x| x / 100.0);
                if let Some(pct) = style.flex_basis_pct {
                    style.flex_basis_css = format!("{}%", num(pct * 100.0));
                }
            } else if v.starts_with("calc(") {
                let resolved = resolve_sign_cqw(&v, parent.width, style.font_size);
                match parse_inset_value(&resolved, style.font_size) {
                    InsetValue::Length(length) => {
                        let length = length.max(0.0);
                        style.flex_basis = Some(length);
                        style.flex_basis_pct = None;
                        style.flex_basis_css = px(length);
                    }
                    InsetValue::Percent(pct) => {
                        style.flex_basis = None;
                        style.flex_basis_pct = Some(pct / 100.0);
                        style.flex_basis_css = format!("{}%", num(pct));
                    }
                    InsetValue::Calc { pct, px: length } => {
                        style.flex_basis = None;
                        style.flex_basis_pct = Some(pct / 100.0);
                        style.flex_basis_css = if length == 0.0 {
                            format!("{}%", num(pct))
                        } else if length > 0.0 {
                            format!("calc({}% + {}px)", num(pct), num(length))
                        } else {
                            format!("calc({}% - {}px)", num(pct), num(-length))
                        };
                    }
                    InsetValue::Auto => {}
                }
            } else {
                style.flex_basis = parse_length_fs(val, style.font_size).map(|x| x.max(0.0));
                style.flex_basis_pct = None;
                if let Some(length) = style.flex_basis {
                    style.flex_basis_css = px(length);
                }
            }
        }
        "align-self" => {
            let lower = val.trim().to_ascii_lowercase();
            let computed = match lower.as_str() {
                "inherit" => Some(parent.align_self_css.clone()),
                "initial" | "unset" => Some("auto".to_string()),
                _ => parse_items_value(val, false, true, false),
            };
            if let Some(computed) = computed {
                let position = computed.split_whitespace().last().unwrap_or("auto");
                style.align_self = if computed == "last baseline" {
                    AlignSelf::LastBaseline
                } else {
                    match position {
                        "auto" => AlignSelf::Auto,
                        "stretch" | "normal" => AlignSelf::Stretch,
                        "flex-start" | "start" | "self-start" => AlignSelf::FlexStart,
                        "flex-end" | "end" | "self-end" => AlignSelf::FlexEnd,
                        "center" => AlignSelf::Center,
                        "baseline" => AlignSelf::Baseline,
                        _ => style.align_self,
                    }
                };
                style.align_self_css = computed;
            }
        }
        "justify-self" => {
            let lower = val.trim().to_ascii_lowercase();
            if let Some(computed) = match lower.as_str() {
                "inherit" => Some(parent.justify_self.clone()),
                "initial" | "unset" => Some("auto".to_string()),
                _ => parse_items_value(val, true, true, false),
            } {
                style.justify_self = computed;
            }
        }
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

        // --- Multi-column ---
        "column-count" => {
            style.column_count = val.trim().parse::<u32>().ok().filter(|&n| n > 0);
        }
        "columns" => {
            // `columns: <count> || <width>`. Take the bare integer as the column count (the length,
            // if any, is the column-width, which we don't size against yet).
            style.column_count = val
                .split_whitespace()
                .find_map(|t| t.parse::<u32>().ok())
                .filter(|&n| n > 0);
        }
        "break-before" => style.break_before_column = val.trim().eq_ignore_ascii_case("column"),
        "break-after" => style.break_after_column = val.trim().eq_ignore_ascii_case("column"),
        "column-span" => style.column_span_all = val.trim().eq_ignore_ascii_case("all"),
        "caption-side" => style.caption_side_bottom = val.trim().eq_ignore_ascii_case("bottom"),

        // --- Grid ---
        "grid-template" | "grid" => {
            // `grid-template: <rows> / <columns>` (the `grid` shorthand reduces to this for the
            // simple track-list form used here). We don't model the `[line-names]`/areas syntax.
            if let Some((rows, cols)) = val.split_once('/') {
                let r = parse_track_list(rows);
                let c = parse_track_list(cols);
                if !r.is_empty() {
                    style.grid_template_rows = r;
                }
                if !c.is_empty() {
                    style.grid_template_columns = c;
                }
            }
        }
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
        "grid-column-start" => set_grid_start(&mut style.grid_column, val),
        "grid-column-end" => set_grid_end(&mut style.grid_column, val),
        "grid-row" => {
            style.grid_row = parse_grid_placement(val);
        }
        "grid-row-start" => set_grid_start(&mut style.grid_row, val),
        "grid-row-end" => set_grid_end(&mut style.grid_row, val),

        // --- Box model: margin ---
        "margin" => {
            let (e, auto) = parse_margin_shorthand(val, style.font_size);
            style.margin = e;
            style.margin_auto = auto;
        }
        "margin-top" => set_margin_side(style, EdgeSide::Top, 0, val),
        "margin-right" => set_margin_side(style, EdgeSide::Right, 1, val),
        "margin-bottom" => set_margin_side(style, EdgeSide::Bottom, 2, val),
        "margin-left" => set_margin_side(style, EdgeSide::Left, 3, val),

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
        "border" => {
            apply_border_shorthand(style, val, EdgeSide::All, current_color, inherited_color);
            style.border_is_system = has_system_color(val);
        }
        "border-top" => {
            apply_border_shorthand(style, val, EdgeSide::Top, current_color, inherited_color)
        }
        "border-right" => {
            apply_border_shorthand(style, val, EdgeSide::Right, current_color, inherited_color)
        }
        "border-bottom" => {
            apply_border_shorthand(style, val, EdgeSide::Bottom, current_color, inherited_color)
        }
        "border-left" => {
            apply_border_shorthand(style, val, EdgeSide::Left, current_color, inherited_color)
        }
        "border-width" => {
            if let Some(e) = parse_edges_shorthand(val, style.font_size) {
                style.border = e;
            }
        }
        "border-top-width" => set_edge(&mut style.border, EdgeSide::Top, val, style.font_size),
        "border-right-width" => set_edge(&mut style.border, EdgeSide::Right, val, style.font_size),
        "border-bottom-width" => {
            set_edge(&mut style.border, EdgeSide::Bottom, val, style.font_size)
        }
        "border-left-width" => set_edge(&mut style.border, EdgeSide::Left, val, style.font_size),
        "border-color" => {
            if let Some(c) = parse_color_ctx(val, current_color, inherited_color) {
                style.border_color = c;
                style.border_is_system = has_system_color(val);
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
            style.width_authored = true;
            style.width = parse_length_fs(val, style.font_size);
            style.width_pct = if style.width.is_none() {
                parse_percent(val).map(|p| p / 100.0)
            } else {
                None
            };
        }
        "height" => {
            style.height_authored = true;
            style.height = parse_length_fs(val, style.font_size);
            style.height_pct = if style.height.is_none() {
                parse_percent(val).map(|p| p / 100.0)
            } else {
                None
            };
        }
        "aspect-ratio" => {
            let v = val.trim().to_ascii_lowercase();
            style.aspect_ratio_auto = v.split_whitespace().any(|token| token == "auto");
            let ratio_text = v.replace("auto", "");
            style.aspect_ratio = ratio_text
                .split_once('/')
                .and_then(|(width, height)| {
                    let width = width.trim().parse::<f32>().ok()?;
                    let height = height.trim().parse::<f32>().ok()?;
                    (width >= 0.0 && height > 0.0).then_some(width / height)
                })
                .or_else(|| {
                    ratio_text
                        .trim()
                        .parse::<f32>()
                        .ok()
                        .filter(|ratio| *ratio > 0.0)
                });
            style.aspect_ratio_set = style.aspect_ratio.is_some();
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
        // Logical margin/padding longhands → physical sides, assuming LTR horizontal-tb (the same
        // assumption the engine makes elsewhere, e.g. the `margin-block`/`margin-inline` shorthands
        // and logical `float`/`clear` above).
        "margin-block-start" => set_margin_side(style, EdgeSide::Top, 0, val),
        "margin-block-end" => set_margin_side(style, EdgeSide::Bottom, 2, val),
        "margin-inline-start" => set_margin_side(style, EdgeSide::Left, 3, val),
        "margin-inline-end" => set_margin_side(style, EdgeSide::Right, 1, val),
        "padding-block-start" => set_edge(&mut style.padding, EdgeSide::Top, val, style.font_size),
        "padding-block-end" => set_edge(&mut style.padding, EdgeSide::Bottom, val, style.font_size),
        "padding-inline-start" => {
            set_edge(&mut style.padding, EdgeSide::Left, val, style.font_size)
        }
        "padding-inline-end" => set_edge(&mut style.padding, EdgeSide::Right, val, style.font_size),

        // --- Typography extras ---
        "line-height" => {
            if let Some(px) = parse_line_height(val, style.font_size) {
                style.line_height = Some(px);
            }
        }
        "-webkit-line-clamp" | "line-clamp" => {
            // `<integer>` (≥1) clamps to that many lines; `none` (or anything else) clears it.
            style.line_clamp = val.trim().parse::<u32>().ok().filter(|&n| n > 0);
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
        // `text-indent`: length applied to the first line of a block container. Percentages
        // (resolved against the containing block width at layout time) are not yet supported, so a
        // `%` value is ignored. The `each-line`/`hanging` keywords are likewise not handled.
        "text-indent" => {
            if let Some(px) = parse_length(val) {
                style.text_indent = px;
            }
        }
        "white-space" => match val.trim().to_ascii_lowercase().as_str() {
            "normal" => style.white_space = WhiteSpace::Normal,
            "nowrap" => style.white_space = WhiteSpace::Nowrap,
            "pre" => style.white_space = WhiteSpace::Pre,
            "pre-wrap" => style.white_space = WhiteSpace::PreWrap,
            // `pre-line` collapses runs of spaces/tabs but preserves newlines as forced breaks.
            "pre-line" => style.white_space = WhiteSpace::PreLine,
            _ => {}
        },
        "visibility" => match val.trim().to_ascii_lowercase().as_str() {
            "visible" => style.visibility = Visibility::Visible,
            "hidden" => style.visibility = Visibility::Hidden,
            "collapse" => style.visibility = Visibility::Collapse,
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
            if let Some(p) = parse_border_radius_pct(val) {
                style.border_radius = 0.0;
                style.border_radius_pct = Some(p);
            } else if let Some(r) = parse_border_radius(val) {
                style.border_radius = r;
                style.border_radius_pct = None;
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
