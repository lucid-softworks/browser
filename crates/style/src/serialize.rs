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
///
/// Returns `None` when the list is invalid per `<family-name>` grammar, so the caller drops the
/// declaration rather than storing a mangled value. The grammar requires each comma component to be
/// *either* a single `<string>` *or* a sequence of `<custom-ident>`s — a quoted string with anything
/// other than whitespace after its closing quote (e.g. `"times" new roman`), or an unterminated
/// quote, is a syntax error. Naively slicing such a component (`body = chars[1..len-1]`) leaves an
/// unbalanced quote that re-escapes and grows on every re-serialization, so rejecting it outright is
/// both spec-correct and what keeps repeated CSSOM round-trips from blowing up.
pub fn serialize_font_family(val: &str) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    for part in split_top_level_commas(val) {
        let fam = part.trim();
        if fam.is_empty() {
            continue;
        }
        let chars: Vec<char> = fam.chars().collect();
        let first = chars[0];
        if first == '"' || first == '\'' {
            // Find the matching closing quote, honouring backslash escapes.
            let mut k = 1;
            let mut closed = false;
            while k < chars.len() {
                if chars[k] == '\\' {
                    k += 2;
                    continue;
                }
                if chars[k] == first {
                    closed = true;
                    break;
                }
                k += 1;
            }
            // Unterminated, or trailing non-whitespace after the closing quote: invalid list.
            if !closed || chars[k + 1..].iter().any(|c| !c.is_whitespace()) {
                return None;
            }
            // Decode escapes to the literal string value before re-escaping, so serialization is
            // idempotent (`'\"x'` -> body `"x` -> `"\"x"` -> body `"x`). Without this, raw
            // backslashes are re-escaped and accumulate on every re-serialization.
            let body = unescape_css_string(&chars[1..k]);
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
    Some(out.join(", "))
}

/// Decode CSS string escapes in `chars` to their literal characters: `\<hex>` (1-6 hex digits, with
/// an optional single trailing whitespace) -> the code point; `\c` for any other char -> `c`. Used
/// to recover the actual string value of a quoted `<family-name>` body before re-serializing it.
fn unescape_css_string(chars: &[char]) -> String {
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            let nx = chars[i + 1];
            if nx.is_ascii_hexdigit() {
                let mut hex = String::new();
                i += 1;
                while i < chars.len() && hex.len() < 6 && chars[i].is_ascii_hexdigit() {
                    hex.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() && chars[i].is_whitespace() {
                    i += 1; // consume one trailing whitespace
                }
                let cp = u32::from_str_radix(&hex, 16).unwrap_or(0);
                out.push(if cp == 0 {
                    '\u{FFFD}'
                } else {
                    char::from_u32(cp).unwrap_or('\u{FFFD}')
                });
                continue;
            }
            out.push(nx);
            i += 2;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
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
