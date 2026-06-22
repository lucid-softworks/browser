use crate::*;
use std::collections::HashMap;

/// Rebuild an element's custom-property (`--name`) environment by cascading its own matching
/// declarations. Used to give a pseudo-element the same `var()` environment as its originating
/// element (the cascade keeps the map internally and doesn't expose it on `ComputedStyle`).
pub(crate) fn element_vars(
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
pub(crate) fn resolve_vars(value: &str, vars: &HashMap<String, String>) -> String {
    resolve_vars_depth(value, vars, 0)
}

pub(crate) const VAR_MAX_DEPTH: usize = 32;

pub(crate) fn resolve_vars_depth(
    value: &str,
    vars: &HashMap<String, String>,
    depth: usize,
) -> String {
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
pub(crate) fn resolve_one_var(args: &str, vars: &HashMap<String, String>, depth: usize) -> String {
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
pub(crate) fn split_first_comma(s: &str) -> (&str, Option<&str>) {
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
