use crate::*;

/// True if a quoted `<family-name>` body must stay quoted (it would otherwise be reinterpreted as a
/// generic-family / CSS-wide keyword / `default`).
pub(crate) fn is_reserved_font_family_word(body: &str) -> bool {
    matches!(
        body.to_ascii_lowercase().as_str(),
        "serif"
            | "sans-serif"
            | "cursive"
            | "fantasy"
            | "monospace"
            | "system-ui"
            | "math"
            | "ui-serif"
            | "ui-sans-serif"
            | "ui-monospace"
            | "ui-rounded"
            | "default"
            | "inherit"
            | "initial"
            | "unset"
            | "revert"
            | "revert-layer"
    )
}

pub(crate) fn is_css_ident_word(w: &str) -> bool {
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
            let body: String = fam
                .chars()
                .skip(1)
                .take(fam.chars().count().saturating_sub(2))
                .collect();
            let words: Vec<&str> = body.split(' ').collect();
            let all_ident = !words.is_empty() && words.iter().all(|w| is_css_ident_word(w));
            let round_trips = all_ident && words.join(" ") == body;
            if round_trips && !is_reserved_font_family_word(&body) {
                out.push(body);
            } else {
                out.push(format!(
                    "\"{}\"",
                    body.replace('\\', "\\\\").replace('"', "\\\"")
                ));
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
pub(crate) fn split_top_level_commas(s: &str) -> Vec<String> {
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
pub(crate) fn serialize_css_string(s: &str) -> String {
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

pub(crate) fn size_constraint_str(c: SizeConstraint) -> String {
    match c {
        SizeConstraint::Px(v) => px(v),
        SizeConstraint::Pct(p) => format!("{}%", num(p * 100.0)),
    }
}

pub(crate) fn justify_content_str(jc: JustifyContent) -> &'static str {
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
pub(crate) fn edges_str(e: Edges) -> String {
    if e.top == e.right && e.right == e.bottom && e.bottom == e.left {
        px(e.top)
    } else if e.top == e.bottom && e.left == e.right {
        format!("{} {}", px(e.top), px(e.right))
    } else {
        format!(
            "{} {} {} {}",
            px(e.top),
            px(e.right),
            px(e.bottom),
            px(e.left)
        )
    }
}

pub(crate) fn track_str(t: TrackSize) -> String {
    match t {
        TrackSize::Px(v) => px(v),
        TrackSize::Fr(v) => format!("{}fr", num(v)),
        TrackSize::Pct(p) => format!("{}%", num(p)),
        TrackSize::Auto => "auto".to_string(),
    }
}

pub(crate) fn tracks_str(tracks: &[TrackSize]) -> String {
    if tracks.is_empty() {
        return "none".to_string();
    }
    tracks
        .iter()
        .map(|t| track_str(*t))
        .collect::<Vec<_>>()
        .join(" ")
}
