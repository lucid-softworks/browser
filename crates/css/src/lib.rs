//! Hand-written CSS parsing.
//!
//! Parses a CSS string into a [`Stylesheet`] of [`Rule`]s. Each rule is a list of raw
//! selector strings (interpreted by the `style` crate) plus a list of `(property, value)`
//! declarations. Comments are stripped, at-rules (`@media`, `@font-face`, …) are consumed
//! gracefully (including any balanced `{ … }` block) without emitting bogus rules, and
//! malformed input never panics.

/// A parsed CSS stylesheet: an ordered list of rules, plus the structured at-rules the cascade
/// needs (registered custom properties from `@property`, and namespace prefix bindings from
/// `@namespace`).
#[derive(Debug, Clone, Default)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
    /// `@property --name { … }` registrations, in source order.
    pub property_rules: Vec<PropertyRule>,
    /// `@namespace [prefix] url(...)` bindings, in source order. An empty `prefix` is the default
    /// namespace.
    pub namespace_rules: Vec<NamespaceRule>,
}

/// A registered custom property from an `@property` rule.
#[derive(Debug, Clone, Default)]
pub struct PropertyRule {
    /// The property name including the leading `--`.
    pub name: String,
    /// The declared `syntax` string with surrounding quotes stripped (e.g. `<length>`, `*`).
    pub syntax: String,
    /// The `inherits` flag (defaults to false if absent / unparseable).
    pub inherits: bool,
    /// The `initial-value`, if declared. `None` when omitted (only valid for the `*` syntax).
    pub initial_value: Option<String>,
}

/// An `@namespace [prefix] <url>` binding.
#[derive(Debug, Clone, Default)]
pub struct NamespaceRule {
    /// The prefix (empty string = default namespace).
    pub prefix: String,
    /// The namespace URI.
    pub uri: String,
}

/// A single rule: a group of selectors and the declarations they apply.
#[derive(Debug, Clone, Default)]
pub struct Rule {
    pub selectors: Vec<String>,
    pub declarations: Vec<(String, String)>,
    /// The raw `@media` query this rule lives under (`None` if not inside any `@media`).
    /// Nested `@media` queries are joined with `" and "`. `@layer`/`@supports` wrappers do
    /// not contribute to this (their inner rules surface with the surrounding media context).
    pub media: Option<String>,
    /// The raw `@container` condition this rule lives under (`None` if not inside any
    /// `@container`). The container name (if any) is dropped — only the `(condition)` part is
    /// retained (joined with `" and "` when nested). Evaluated like `media` by the `style` crate
    /// against an assumed container width.
    pub container: Option<String>,
    /// The absolute URL of the stylesheet that contains this rule, used to resolve relative
    /// `url(...)` values (`mask-image`, `background-image`, …) against the *stylesheet's* own URL
    /// rather than the document URL (per CSS, a relative `url()` is resolved against the stylesheet
    /// it appears in). `None` for sheets parsed via [`parse`] (base unknown); set by
    /// [`parse_with_base`] for `<link>`/`@import`'d sheets and inline `<style>` (document URL).
    pub base_url: Option<String>,
}

/// Parse a CSS string into a [`Stylesheet`].
pub fn parse(css: &str) -> Stylesheet {
    parse_inner(css, None)
}

/// Parse a CSS string, stamping every rule with `base_url` (the absolute URL of the stylesheet this
/// text came from) so relative `url(...)` values can later be resolved against the stylesheet's own
/// URL. See [`Rule::base_url`].
pub fn parse_with_base(css: &str, base_url: &str) -> Stylesheet {
    parse_inner(css, Some(base_url))
}

fn parse_inner(css: &str, base_url: Option<&str>) -> Stylesheet {
    let stripped = strip_comments(css);
    let bytes: Vec<char> = stripped.chars().collect();
    let mut rules = Vec::new();
    parse_rules(&bytes, 0, bytes.len(), None, None, &mut rules);
    if let Some(base) = base_url {
        for rule in &mut rules {
            rule.base_url = Some(base.to_string());
        }
    }
    let (property_rules, namespace_rules) = extract_top_level_at_rules(&bytes);
    Stylesheet { rules, property_rules, namespace_rules }
}

/// Scan the (comment-stripped) source for the `@property` and `@namespace` at-rules the cascade
/// needs. These are top-level (not nested in `@media`/etc.) in practice; a flat scan is sufficient
/// for the CSSOM tests. Returns `(property_rules, namespace_rules)` in source order.
fn extract_top_level_at_rules(bytes: &[char]) -> (Vec<PropertyRule>, Vec<NamespaceRule>) {
    let end = bytes.len();
    let mut props = Vec::new();
    let mut namespaces = Vec::new();
    let mut pos = 0usize;
    while pos < end {
        match bytes[pos] {
            '@' => {
                let name_start = pos + 1;
                let mut i = name_start;
                while i < end && (bytes[i].is_ascii_alphanumeric() || bytes[i] == '-' || bytes[i] == '_') {
                    i += 1;
                }
                let name: String = bytes[name_start..i].iter().collect();
                let name = name.to_ascii_lowercase();
                let prelude_end = scan_to_block_or_semi(bytes, i, end);
                if prelude_end >= end {
                    break;
                }
                if name == "namespace" && bytes[prelude_end] == ';' {
                    let prelude: String = bytes[i..prelude_end].iter().collect();
                    if let Some(ns) = parse_namespace_prelude(prelude.trim()) {
                        namespaces.push(ns);
                    }
                    pos = prelude_end + 1;
                    continue;
                }
                if bytes[prelude_end] == ';' {
                    pos = prelude_end + 1;
                    continue;
                }
                // Block at-rule.
                let body_start = prelude_end + 1;
                let body_end = scan_balanced_block_end(bytes, body_start, end);
                if name == "property" {
                    let prelude: String = bytes[i..prelude_end].iter().collect();
                    let pname = prelude.trim().to_string();
                    if pname.starts_with("--") {
                        let body: String = bytes[body_start..body_end].iter().collect();
                        props.push(parse_property_body(&pname, &body));
                    }
                }
                pos = if body_end < end { body_end + 1 } else { body_end };
            }
            '"' => pos = skip_string(bytes, pos, end, '"'),
            '\'' => pos = skip_string(bytes, pos, end, '\''),
            '{' => pos = scan_balanced_block_end(bytes, pos + 1, end).saturating_add(1),
            _ => pos += 1,
        }
    }
    (props, namespaces)
}

/// Parse a `@namespace` prelude: an optional prefix followed by a `url(...)` or string URI.
fn parse_namespace_prelude(prelude: &str) -> Option<NamespaceRule> {
    let p = prelude.trim();
    // Split into an optional prefix token and the URI part.
    let (prefix, uri_part) = match p.find(|c: char| c.is_whitespace()) {
        Some(idx) => {
            let first = p[..idx].trim();
            let rest = p[idx..].trim();
            // If the first token looks like a url()/string, there's no prefix.
            if first.starts_with("url(") || first.starts_with('"') || first.starts_with('\'') {
                ("", p)
            } else {
                (first, rest)
            }
        }
        None => ("", p),
    };
    let uri = strip_url_or_string(uri_part)?;
    Some(NamespaceRule { prefix: prefix.to_string(), uri })
}

/// Strip a `url(...)` wrapper or string quotes to get the raw URI.
fn strip_url_or_string(s: &str) -> Option<String> {
    let s = s.trim();
    if let Some(inner) = s.strip_prefix("url(").and_then(|r| r.strip_suffix(')')) {
        let inner = inner.trim();
        let inner = inner.strip_prefix('"').and_then(|r| r.strip_suffix('"'))
            .or_else(|| inner.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
            .unwrap_or(inner);
        return Some(inner.trim().to_string());
    }
    if let Some(inner) = s.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        return Some(inner.to_string());
    }
    if let Some(inner) = s.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')) {
        return Some(inner.to_string());
    }
    if s.is_empty() { None } else { Some(s.to_string()) }
}

/// Parse the descriptor body of an `@property --x { … }` rule.
fn parse_property_body(name: &str, body: &str) -> PropertyRule {
    let mut rule = PropertyRule { name: name.to_string(), ..Default::default() };
    for (k, v) in parse_declarations(body) {
        match k.as_str() {
            "syntax" => {
                let v = v.trim();
                let v = v.strip_prefix('"').and_then(|r| r.strip_suffix('"'))
                    .or_else(|| v.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
                    .unwrap_or(v);
                rule.syntax = v.trim().to_string();
            }
            "inherits" => rule.inherits = v.trim().eq_ignore_ascii_case("true"),
            "initial-value" => rule.initial_value = Some(v.trim().to_string()),
            _ => {}
        }
    }
    rule
}

/// Combine an existing at-rule context (`media` or `container`) with a newly entered query,
/// joining nested conditions with `" and "`.
fn combine_query(existing: Option<&str>, new: &str) -> String {
    match existing {
        Some(m) if !m.is_empty() => format!("{m} and {new}"),
        _ => new.to_string(),
    }
}

/// Strip an optional leading container name from an `@container` prelude, returning just the
/// `(condition …)` part. `@container sidebar (min-width: 400px)` → `(min-width: 400px)`;
/// `@container (min-width: 400px)` is returned unchanged. If there's no `(`, returns the prelude
/// trimmed (an unrecognized/`style(...)`-style query the cascade will treat permissively).
fn strip_container_name(prelude: &str) -> String {
    let p = prelude.trim();
    match p.find('(') {
        Some(idx) => p[idx..].trim().to_string(),
        None => p.to_string(),
    }
}

/// Parse the rules within `bytes[start..end]`, appending them to `out`. `media` is the media
/// query currently in scope (from enclosing `@media` blocks). At-rules with blocks (`@media`,
/// `@supports`, `@layer name { … }`) recurse; other at-rules are consumed/skipped.
fn parse_rules(
    bytes: &[char],
    start: usize,
    end: usize,
    media: Option<&str>,
    container: Option<&str>,
    out: &mut Vec<Rule>,
) {
    let mut pos = start;
    while pos < end {
        // Skip leading whitespace.
        while pos < end && bytes[pos].is_whitespace() {
            pos += 1;
        }
        if pos >= end {
            break;
        }

        if bytes[pos] == '@' {
            pos = parse_at_rule(bytes, pos, end, media, container, out);
            continue;
        }

        if bytes[pos] == '}' {
            // Stray close brace; skip it.
            pos += 1;
            continue;
        }

        // Read the selector prelude up to the next top-level `{` (or end / stray `}`),
        // skipping over balanced parens and strings so `:is(a, b)` etc. don't confuse us.
        let prelude_start = pos;
        pos = scan_to_block_or_semi(bytes, pos, end);

        if pos >= end {
            // Dangling prelude with no block — ignore.
            break;
        }
        if bytes[pos] == '}' {
            // No declaration block — skip the `}` and ignore the dangling prelude.
            pos += 1;
            continue;
        }
        if bytes[pos] == ';' {
            // A statement with no block at rule level (unusual); skip it.
            pos += 1;
            continue;
        }

        let prelude: String = bytes[prelude_start..pos].iter().collect();
        // Skip the `{`.
        pos += 1;

        // Read the declaration block body up to the matching `}` (balanced).
        let body_start = pos;
        let body_end = scan_balanced_block_end(bytes, pos, end);
        pos = if body_end < end { body_end + 1 } else { body_end };

        let selectors = parse_selector_list(&prelude);
        if !selectors.is_empty() {
            // The body may contain BOTH declarations and nested rule blocks (CSS Nesting). Parse
            // it splitting the two, then flatten nested rules against this rule's selectors.
            parse_rule_body(bytes, body_start, body_end, &selectors, media, container, out);
        }
    }
}

/// Parse a rule body (`bytes[start..end]`) that may interleave declarations and nested rule
/// blocks. `parent_selectors` is the (already comma-expanded) selector list of the enclosing
/// rule; nested selectors are combined against it (`&` substitution / bare-descendant). The
/// produced flat [`Rule`]s are appended to `out`, preserving source order as much as practical:
/// all declarations collected directly in this body form one rule (emitted at the point its
/// run begins), and each nested block is flattened in place.
// The final `flush_decls!()` after the loop assigns `own_rule_idx` it then never reads; that
// dead write is intentional (the macro is shared with in-loop flushes that DO read it).
#[allow(unused_assignments)]
fn parse_rule_body(
    bytes: &[char],
    start: usize,
    end: usize,
    parent_selectors: &[String],
    media: Option<&str>,
    container: Option<&str>,
    out: &mut Vec<Rule>,
) {
    let mut pos = start;
    let mut decl_buf = String::new();
    // Index in `out` of this body's own declaration rule (created lazily on first declaration).
    let mut own_rule_idx: Option<usize> = None;

    // Flush accumulated declaration text into this body's own rule.
    macro_rules! flush_decls {
        () => {{
            let decls = parse_declarations(&decl_buf);
            decl_buf.clear();
            if !decls.is_empty() {
                match own_rule_idx {
                    Some(i) => out[i].declarations.extend(decls),
                    None => {
                        own_rule_idx = Some(out.len());
                        out.push(Rule {
                            selectors: parent_selectors.to_vec(),
                            declarations: decls,
                            media: media.map(str::to_string),
                            container: container.map(str::to_string),
                            base_url: None,
                        });
                    }
                }
            }
        }};
    }

    while pos < end {
        while pos < end && bytes[pos].is_whitespace() {
            pos += 1;
        }
        if pos >= end {
            break;
        }

        // A nested at-rule (e.g. `@media (...) { ... }`) inside a rule body.
        if bytes[pos] == '@' {
            flush_decls!();
            pos = parse_nested_at_rule(bytes, pos, end, parent_selectors, media, container, out);
            continue;
        }

        // Scan to the next top-level `;`, `{`, or `}`.
        let seg_start = pos;
        pos = scan_to_decl_or_block(bytes, pos, end);
        if pos >= end {
            // Trailing declaration text with no terminator.
            decl_buf.push_str(&bytes[seg_start..end].iter().collect::<String>());
            break;
        }
        match bytes[pos] {
            ';' => {
                // A declaration (or empty). Accumulate including the `;` for parse_declarations.
                decl_buf.push_str(&bytes[seg_start..pos].iter().collect::<String>());
                decl_buf.push(';');
                pos += 1;
            }
            '}' => {
                // Stray close inside the body; treat preceding text as a final declaration.
                decl_buf.push_str(&bytes[seg_start..pos].iter().collect::<String>());
                pos += 1;
            }
            '{' => {
                // A nested rule block. Flush pending declarations first so source order (and thus
                // cascade order) is preserved between this body's own rule and the nested rules.
                flush_decls!();
                // Any declarations after this nested block start a fresh own-rule (later in source
                // order than the nested rules), rather than merging into the pre-block rule.
                own_rule_idx = None;
                // The text before `{` is its selector prelude.
                let prelude: String = bytes[seg_start..pos].iter().collect();
                pos += 1;
                let nbody_start = pos;
                let nbody_end = scan_balanced_block_end(bytes, pos, end);
                pos = if nbody_end < end { nbody_end + 1 } else { nbody_end };

                let nested_sel = parse_selector_list(&prelude);
                if !nested_sel.is_empty() {
                    let combined = combine_selectors(parent_selectors, &nested_sel);
                    // Recurse: the nested body may itself interleave declarations / nesting.
                    parse_rule_body(bytes, nbody_start, nbody_end, &combined, media, container, out);
                }
            }
            _ => unreachable!(),
        }
    }
    flush_decls!();
}

/// Handle an at-rule nested inside a rule body. `@media`/`@supports` blocks are unwrapped: their
/// inner content is parsed as a rule body against `parent_selectors`, with `@media` extending the
/// media context. Other at-rules are consumed/skipped. Returns the position past the at-rule.
fn parse_nested_at_rule(
    bytes: &[char],
    start: usize,
    end: usize,
    parent_selectors: &[String],
    media: Option<&str>,
    container: Option<&str>,
    out: &mut Vec<Rule>,
) -> usize {
    let mut i = start + 1;
    let name_start = i;
    while i < end && (bytes[i].is_ascii_alphanumeric() || bytes[i] == '-' || bytes[i] == '_') {
        i += 1;
    }
    let name: String = bytes[name_start..i].iter().collect::<String>().to_ascii_lowercase();

    let prelude_start = i;
    let prelude_end = scan_to_block_or_semi(bytes, i, end);
    if prelude_end >= end {
        return prelude_end;
    }
    if bytes[prelude_end] == ';' {
        return prelude_end + 1;
    }

    let body_start = prelude_end + 1;
    let body_end = scan_balanced_block_end(bytes, body_start, end);
    let next = if body_end < end { body_end + 1 } else { body_end };
    let prelude: String = bytes[prelude_start..prelude_end].iter().collect();
    let prelude = prelude.trim();

    match name.as_str() {
        "media" => {
            let combined = combine_query(media, prelude);
            parse_rule_body(bytes, body_start, body_end, parent_selectors, Some(&combined), container, out);
        }
        "container" => {
            let cond = strip_container_name(prelude);
            let combined = combine_query(container, &cond);
            parse_rule_body(bytes, body_start, body_end, parent_selectors, media, Some(&combined), out);
        }
        "supports" => {
            parse_rule_body(bytes, body_start, body_end, parent_selectors, media, container, out);
        }
        _ => {}
    }
    next
}

/// Scan from `pos` to the first top-level `;`, `{`, or `}` (or `end`), skipping balanced
/// `(…)`/`[…]` and string literals.
fn scan_to_decl_or_block(bytes: &[char], mut pos: usize, end: usize) -> usize {
    while pos < end {
        match bytes[pos] {
            ';' | '{' | '}' => return pos,
            '(' => pos = skip_balanced(bytes, pos, end, '(', ')'),
            '[' => pos = skip_balanced(bytes, pos, end, '[', ']'),
            '"' => pos = skip_string(bytes, pos, end, '"'),
            '\'' => pos = skip_string(bytes, pos, end, '\''),
            _ => pos += 1,
        }
    }
    pos
}

/// Combine a parent selector list with a nested selector list per CSS Nesting, expanding both
/// comma lists combinatorially. For each (parent, nested) pair: every `&` in the nested selector
/// is replaced by the parent; a nested selector with no `&` becomes `parent nested` (descendant).
fn combine_selectors(parents: &[String], nested: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(parents.len() * nested.len());
    for child in nested {
        for parent in parents {
            out.push(combine_one(parent, child));
        }
    }
    out
}

/// Combine a single parent selector with a single nested selector. `&` (top-level, not inside a
/// string) is replaced by `parent`; if the nested selector contains no top-level `&`, it's
/// treated as a descendant (`parent child`).
fn combine_one(parent: &str, child: &str) -> String {
    let chars: Vec<char> = child.chars().collect();
    let mut has_amp = false;
    let mut out = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0i32;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                out.push(c);
            }
            '(' | '[' => {
                depth += 1;
                out.push(c);
            }
            ')' | ']' => {
                depth = (depth - 1).max(0);
                out.push(c);
            }
            '&' if depth == 0 => {
                has_amp = true;
                out.push_str(parent);
            }
            _ => out.push(c),
        }
        i += 1;
    }
    if has_amp {
        out
    } else {
        format!("{parent} {}", out.trim())
    }
}

/// Extract the URL specifiers of top-level `@import` rules in source order. Handles
/// `@import "url";`, `@import 'url';`, `@import url("url");`, `@import url(url);`, and an optional
/// trailing media query (`@import "x" screen;`) — only the URL is returned. `@import` rules inside
/// any `{ … }` block are ignored (only top level / not inside another at-rule's block).
pub fn extract_imports(css: &str) -> Vec<String> {
    let stripped = strip_comments(css);
    let chars: Vec<char> = stripped.chars().collect();
    let bytes: &[char] = &chars;
    let end = bytes.len();
    let mut out = Vec::new();
    let mut pos = 0usize;

    while pos < end {
        while pos < end && bytes[pos].is_whitespace() {
            pos += 1;
        }
        if pos >= end {
            break;
        }
        if bytes[pos] == '@' {
            // Read the at-rule name.
            let mut i = pos + 1;
            let name_start = i;
            while i < end && (bytes[i].is_ascii_alphanumeric() || bytes[i] == '-' || bytes[i] == '_')
            {
                i += 1;
            }
            let name: String =
                bytes[name_start..i].iter().collect::<String>().to_ascii_lowercase();
            let prelude_end = scan_to_block_or_semi(bytes, i, end);
            if name == "import" && prelude_end < end && bytes[prelude_end] == ';' {
                let prelude: String = bytes[i..prelude_end].iter().collect();
                if let Some(u) = parse_import_specifier(&prelude) {
                    out.push(u);
                }
            }
            // Advance past this at-rule (skip its block if it has one).
            if prelude_end >= end {
                break;
            }
            if bytes[prelude_end] == ';' {
                pos = prelude_end + 1;
            } else {
                // Block at-rule: skip its balanced body so nested `@import`s aren't surfaced.
                let body_end = scan_balanced_block_end(bytes, prelude_end + 1, end);
                pos = if body_end < end { body_end + 1 } else { body_end };
            }
            continue;
        }
        // A normal rule: skip its prelude and balanced block.
        pos = scan_to_block_or_semi(bytes, pos, end);
        if pos >= end {
            break;
        }
        match bytes[pos] {
            '{' => {
                let body_end = scan_balanced_block_end(bytes, pos + 1, end);
                pos = if body_end < end { body_end + 1 } else { body_end };
            }
            _ => pos += 1, // `;` or stray `}`
        }
    }
    out
}

/// Parse the prelude of an `@import` rule (everything after `@import`, before the `;`) into its
/// URL string. Accepts `"url"`, `'url'`, `url("url")`, `url('url')`, `url(bare)`, with optional
/// trailing media query (ignored). Returns `None` if no URL is found.
fn parse_import_specifier(prelude: &str) -> Option<String> {
    let s = prelude.trim();
    let lower = s.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("url(") {
        // Find the matching `)` in the original (case-preserving) string.
        let open = s.len() - rest.len() - 1 + 1; // index just past `url(`
        let close_rel = s[open..].find(')')?;
        let inner = s[open..open + close_rel].trim();
        Some(unquote(inner))
    } else if s.starts_with('"') || s.starts_with('\'') {
        let quote = s.chars().next().unwrap();
        let rest = &s[1..];
        let close = rest.find(quote)?;
        Some(rest[..close].to_string())
    } else {
        None
    }
}

/// Strip surrounding matching quotes from a string, if present.
fn unquote(s: &str) -> String {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Parse an at-rule starting at `start` (`bytes[start] == '@'`). `@media`/`@supports`/`@layer`
/// with a block recurse into their body; statement at-rules (ending in `;`) and unknown
/// block at-rules (`@font-face`, `@keyframes`, `@property`, …) are consumed and skipped.
/// Returns the position just past the at-rule.
fn parse_at_rule(
    bytes: &[char],
    start: usize,
    end: usize,
    media: Option<&str>,
    container: Option<&str>,
    out: &mut Vec<Rule>,
) -> usize {
    // Read the at-rule name (the identifier after `@`).
    let mut i = start + 1;
    let name_start = i;
    while i < end
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == '-' || bytes[i] == '_')
    {
        i += 1;
    }
    let name: String = bytes[name_start..i].iter().collect();
    let name = name.to_ascii_lowercase();

    // Find the prelude end: the first top-level `{` or `;`.
    let prelude_start = i;
    let prelude_end = scan_to_block_or_semi(bytes, i, end);

    if prelude_end >= end {
        return prelude_end;
    }

    if bytes[prelude_end] == ';' {
        // Statement at-rule (e.g. `@layer a, b, c;`, `@import …;`, `@charset …;`). Skip.
        return prelude_end + 1;
    }

    // Block at-rule. Locate the balanced block body.
    let body_start = prelude_end + 1;
    let body_end = scan_balanced_block_end(bytes, body_start, end);
    let next = if body_end < end { body_end + 1 } else { body_end };

    let prelude: String = bytes[prelude_start..prelude_end].iter().collect();
    let prelude = prelude.trim();

    match name.as_str() {
        "media" => {
            // Combine the existing media context with this query.
            let combined = combine_query(media, prelude);
            parse_rules(bytes, body_start, body_end, Some(&combined), container, out);
        }
        "container" => {
            // Treat `@container [name] (cond)` like `@media`: drop the optional name, retain the
            // `(condition)` and tag inner rules so the cascade can evaluate it.
            let cond = strip_container_name(prelude);
            let combined = combine_query(container, &cond);
            parse_rules(bytes, body_start, body_end, media, Some(&combined), out);
        }
        "supports" | "layer" => {
            // Unwrap: parse the inner content as normal rules, preserving media/container context.
            parse_rules(bytes, body_start, body_end, media, container, out);
        }
        // @font-face, @keyframes, @property, @page, @counter-style, @font-feature-values, …
        // Consume and skip the block (no rules emitted).
        _ => {}
    }

    next
}

/// Scan forward from `pos` to the first top-level `{` or `;` (or `}` / `end`), skipping over
/// balanced `(…)` and `[…]`, and string literals. Returns the index of the stopping char (or
/// `end`).
fn scan_to_block_or_semi(bytes: &[char], mut pos: usize, end: usize) -> usize {
    while pos < end {
        match bytes[pos] {
            '{' | ';' | '}' => return pos,
            '(' => pos = skip_balanced(bytes, pos, end, '(', ')'),
            '[' => pos = skip_balanced(bytes, pos, end, '[', ']'),
            '"' => pos = skip_string(bytes, pos, end, '"'),
            '\'' => pos = skip_string(bytes, pos, end, '\''),
            _ => pos += 1,
        }
    }
    pos
}

/// Given `pos` pointing just inside a `{` block (at the first body char), scan to the matching
/// close `}` at the same nesting depth, skipping nested blocks, parens, and strings. Returns
/// the index of the matching `}` (or `end` if unbalanced).
fn scan_balanced_block_end(bytes: &[char], mut pos: usize, end: usize) -> usize {
    let mut depth = 0usize;
    while pos < end {
        match bytes[pos] {
            '}' => {
                if depth == 0 {
                    return pos;
                }
                depth -= 1;
                pos += 1;
            }
            '{' => {
                depth += 1;
                pos += 1;
            }
            '(' => pos = skip_balanced(bytes, pos, end, '(', ')'),
            '[' => pos = skip_balanced(bytes, pos, end, '[', ']'),
            '"' => pos = skip_string(bytes, pos, end, '"'),
            '\'' => pos = skip_string(bytes, pos, end, '\''),
            _ => pos += 1,
        }
    }
    pos
}

/// Skip a balanced `open … close` run starting at `pos` (which is `open`). Handles nesting and
/// strings inside. Returns the index just past the matching `close` (or `end`).
fn skip_balanced(bytes: &[char], mut pos: usize, end: usize, open: char, close: char) -> usize {
    let mut depth = 0usize;
    while pos < end {
        let c = bytes[pos];
        if c == open {
            depth += 1;
            pos += 1;
        } else if c == close {
            depth -= 1;
            pos += 1;
            if depth == 0 {
                return pos;
            }
        } else if c == '"' {
            pos = skip_string(bytes, pos, end, '"');
        } else if c == '\'' {
            pos = skip_string(bytes, pos, end, '\'');
        } else {
            pos += 1;
        }
    }
    pos
}

/// Skip a string literal starting at `pos` (the opening quote). Returns the index just past
/// the closing quote (or `end`). Handles backslash escapes.
fn skip_string(bytes: &[char], pos: usize, end: usize, quote: char) -> usize {
    let mut i = pos + 1;
    while i < end {
        if bytes[i] == '\\' {
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            return i + 1;
        }
        i += 1;
    }
    i
}

/// Parse an inline `style="..."` attribute value (or any bare declaration block) into a
/// list of `(property, value)` pairs. Property names are lowercased; values are trimmed
/// and kept as-is.
pub fn parse_declarations(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for chunk in split_top_level(s, ';') {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        // Split on the FIRST top-level colon (values may contain `:` inside `url(...)` etc.).
        if let Some(idx) = find_top_level(chunk, ':') {
            let raw = chunk[..idx].trim();
            // Custom properties (`--name`) are case-sensitive; everything else is lowercased.
            let prop = if raw.starts_with("--") {
                raw.to_string()
            } else {
                raw.to_ascii_lowercase()
            };
            let val = chunk[idx + 1..].trim().to_string();
            if !prop.is_empty() && !val.is_empty() {
                out.push((prop, val));
            }
        }
    }
    out
}

/// Split `s` on top-level occurrences of `sep` (not inside `(…)`, `[…]`, or string literals).
fn split_top_level(s: &str, sep: char) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '(' | '[' => depth += 1,
            ')' | ']' => depth = (depth - 1).max(0),
            _ if c == sep && depth == 0 => {
                parts.push(chars[start..i].iter().collect());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(chars[start..].iter().collect());
    parts
}

/// Find the index (in chars-as-bytes terms via char_indices) of the first top-level `sep`.
fn find_top_level(s: &str, sep: char) -> Option<usize> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut prev_backslash = false;
    for (idx, c) in s.char_indices() {
        if let Some(q) = quote {
            if prev_backslash {
                prev_backslash = false;
            } else if c == '\\' {
                prev_backslash = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '(' | '[' => depth += 1,
            ')' | ']' => depth = (depth - 1).max(0),
            _ if c == sep && depth == 0 => return Some(idx),
            _ => {}
        }
    }
    None
}

/// Split a comma-separated selector list into trimmed, non-empty raw selector strings.
///
/// Splits on *top-level* commas only (respecting `()`, `[]`, and quotes), so functional
/// pseudo-classes with comma-separated arguments — `:is(.a, .b)`, `:not(h1, h2)`,
/// `:nth-child(2n, …)` — and attribute values containing commas survive intact.
fn parse_selector_list(s: &str) -> Vec<String> {
    split_top_level(s, ',')
        .into_iter()
        .map(|sel| sel.trim().to_string())
        .filter(|sel| !sel.is_empty())
        .collect()
}

/// Strip `/* ... */` comments. Unterminated comments swallow the rest of the input.
fn strip_comments(css: &str) -> String {
    let chars: Vec<char> = css.chars().collect();
    let mut out = String::with_capacity(css.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            // Find the terminating `*/`.
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            // Skip the closing `*/` if present.
            i = (i + 2).min(chars.len());
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_parse_has_no_rules() {
        let sheet = parse("");
        assert_eq!(sheet.rules.len(), 0);
    }

    #[test]
    fn extracts_property_rules() {
        let sheet = parse(
            "@property --x { syntax: \"<length>\"; inherits: true; initial-value: 0px; } \
             @property --y { syntax: '*'; inherits: false; } \
             div { color: red }",
        );
        assert_eq!(sheet.property_rules.len(), 2);
        let x = &sheet.property_rules[0];
        assert_eq!(x.name, "--x");
        assert_eq!(x.syntax, "<length>");
        assert!(x.inherits);
        assert_eq!(x.initial_value.as_deref(), Some("0px"));
        let y = &sheet.property_rules[1];
        assert_eq!(y.name, "--y");
        assert_eq!(y.syntax, "*");
        assert!(!y.inherits);
        assert_eq!(y.initial_value, None);
        // The non-at-rule still parses.
        assert_eq!(sheet.rules.len(), 1);
    }

    #[test]
    fn extracts_namespace_rules() {
        let sheet = parse(
            "@namespace url(http://www.w3.org/1999/xhtml); \
             @namespace svg url(http://www.w3.org/2000/svg); \
             svg|rect { fill: red }",
        );
        assert_eq!(sheet.namespace_rules.len(), 2);
        assert_eq!(sheet.namespace_rules[0].prefix, "");
        assert_eq!(sheet.namespace_rules[0].uri, "http://www.w3.org/1999/xhtml");
        assert_eq!(sheet.namespace_rules[1].prefix, "svg");
        assert_eq!(sheet.namespace_rules[1].uri, "http://www.w3.org/2000/svg");
    }

    #[test]
    fn parses_multiple_rules() {
        let sheet = parse("h1 { color: red; font-size: 32px } p { color: #00f }");
        assert_eq!(sheet.rules.len(), 2);
        assert_eq!(sheet.rules[0].selectors, vec!["h1"]);
        assert_eq!(
            sheet.rules[0].declarations,
            vec![
                ("color".to_string(), "red".to_string()),
                ("font-size".to_string(), "32px".to_string())
            ]
        );
        assert_eq!(sheet.rules[1].selectors, vec!["p"]);
        assert_eq!(
            sheet.rules[1].declarations,
            vec![("color".to_string(), "#00f".to_string())]
        );
    }

    #[test]
    fn parses_grouped_selectors() {
        let sheet = parse("h1, h2 , .note { color: blue; }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors, vec!["h1", "h2", ".note"]);
    }

    #[test]
    fn strips_comments() {
        let sheet = parse("/* a comment */ p /* mid */ { color: /* x */ red; } /* trailing */");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors, vec!["p"]);
        assert_eq!(
            sheet.rules[0].declarations,
            vec![("color".to_string(), "red".to_string())]
        );
    }

    #[test]
    fn media_at_rule_surfaces_tagged_rules() {
        let sheet = parse(
            "p { color: red } @media (max-width: 600px) { p { color: green } } h1 { color: blue }",
        );
        // All three rules surface now; the nested one carries its media query.
        assert_eq!(sheet.rules.len(), 3);
        assert_eq!(sheet.rules[0].selectors, vec!["p"]);
        assert_eq!(sheet.rules[0].media, None);
        assert_eq!(sheet.rules[1].selectors, vec!["p"]);
        assert_eq!(sheet.rules[1].media.as_deref(), Some("(max-width: 600px)"));
        assert_eq!(sheet.rules[1].declarations, vec![("color".to_string(), "green".to_string())]);
        assert_eq!(sheet.rules[2].selectors, vec!["h1"]);
        assert_eq!(sheet.rules[2].media, None);
    }

    #[test]
    fn layer_block_surfaces_inner_rules() {
        let sheet = parse("@layer utilities { .a { color: #f00 } }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors, vec![".a"]);
        assert_eq!(sheet.rules[0].media, None);
        assert_eq!(sheet.rules[0].declarations, vec![("color".to_string(), "#f00".to_string())]);
    }

    #[test]
    fn bare_layer_statement_yields_no_rules() {
        let sheet = parse("@layer theme, base, components, utilities;");
        assert_eq!(sheet.rules.len(), 0);
    }

    #[test]
    fn nested_layer_then_media_surfaces_rule() {
        let sheet = parse("@layer base { @media (min-width: 640px) { .c { color: #0f0 } } }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors, vec![".c"]);
        assert_eq!(sheet.rules[0].media.as_deref(), Some("(min-width: 640px)"));
    }

    #[test]
    fn media_min_width_tagged() {
        let sheet = parse("@media (min-width: 768px) { .b { color: #0f0 } }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors, vec![".b"]);
        assert_eq!(sheet.rules[0].media.as_deref(), Some("(min-width: 768px)"));
    }

    #[test]
    fn container_at_rule_surfaces_tagged_rule() {
        let sheet = parse("@container (min-width: 400px) { .a { color: red } }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors, vec![".a"]);
        assert_eq!(sheet.rules[0].container.as_deref(), Some("(min-width: 400px)"));
        assert_eq!(sheet.rules[0].media, None);
        assert_eq!(sheet.rules[0].declarations, vec![("color".to_string(), "red".to_string())]);
    }

    #[test]
    fn named_container_drops_name_keeps_condition() {
        let sheet = parse("@container sidebar (min-width: 400px) { .a { color: blue } }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors, vec![".a"]);
        assert_eq!(sheet.rules[0].container.as_deref(), Some("(min-width: 400px)"));
    }

    #[test]
    fn container_nested_inside_media() {
        let sheet = parse(
            "@media (min-width: 640px) { @container (max-width: 700px) { .c { color: #0f0 } } }",
        );
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors, vec![".c"]);
        assert_eq!(sheet.rules[0].media.as_deref(), Some("(min-width: 640px)"));
        assert_eq!(sheet.rules[0].container.as_deref(), Some("(max-width: 700px)"));
    }

    #[test]
    fn container_nested_inside_rule_flattens() {
        let sheet = parse(".a { color: red; @container (min-width: 300px) { color: blue } }");
        assert_eq!(sheet.rules.len(), 2);
        assert_eq!(sheet.rules[0].selectors, vec![".a"]);
        assert_eq!(sheet.rules[0].container, None);
        assert_eq!(sheet.rules[1].selectors, vec![".a"]);
        assert_eq!(sheet.rules[1].container.as_deref(), Some("(min-width: 300px)"));
        assert_eq!(sheet.rules[1].declarations, vec![("color".to_string(), "blue".to_string())]);
    }

    #[test]
    fn supports_block_surfaces_inner_rules() {
        let sheet = parse("@supports (display: grid) { .g { display: grid } }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors, vec![".g"]);
    }

    #[test]
    fn skipped_block_at_rules_emit_nothing() {
        let sheet = parse(
            "@font-face { font-family: x; src: url('a.woff2') } \
             @keyframes spin { from { transform: rotate(0) } to { transform: rotate(360deg) } } \
             @property --p { syntax: '<color>'; inherits: false } \
             p { color: red }",
        );
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors, vec!["p"]);
    }

    #[test]
    fn stress_parens_relative_colors_and_strings_do_not_panic() {
        let css = "@layer utilities { \
            .x { color: rgb(from red r g b); background: var(--c, oklch(0.5 0.1 30)) } \
            .y::before { content: '{ ; not a brace }'; color: hsl(0 100% 50%) } \
            .z { background: url(\"data:image/svg+xml;base64,abc{}\") } \
            @media (min-width: 1024px) and (max-width: 1280px) { .w { width: calc(100% - 2rem) } } \
        }";
        let sheet = parse(css);
        // Should surface .x, .y::before, .z, and .w without panicking.
        assert!(sheet.rules.iter().any(|r| r.selectors.iter().any(|s| s == ".x")));
        assert!(sheet.rules.iter().any(|r| r.selectors.iter().any(|s| s == ".z")));
        assert!(sheet
            .rules
            .iter()
            .any(|r| r.selectors.iter().any(|s| s == ".w") && r.media.is_some()));
    }

    #[test]
    fn skips_at_rule_with_semicolon() {
        let sheet = parse("@import url(\"x.css\"); p { color: red }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors, vec!["p"]);
    }

    #[test]
    fn parse_with_base_stamps_every_rule() {
        let sheet = parse_with_base(
            "a { color: red } @media (min-width: 1px) { b { color: blue } }",
            "https://example.com/css/app.css",
        );
        assert_eq!(sheet.rules.len(), 2);
        for r in &sheet.rules {
            assert_eq!(r.base_url.as_deref(), Some("https://example.com/css/app.css"));
        }
        // Plain `parse` leaves the base unset.
        assert!(parse("a { color: red }").rules[0].base_url.is_none());
    }

    #[test]
    fn parses_inline_declarations() {
        let decls = parse_declarations("color: red; font-size: 20px ; font-weight:bold");
        assert_eq!(
            decls,
            vec![
                ("color".to_string(), "red".to_string()),
                ("font-size".to_string(), "20px".to_string()),
                ("font-weight".to_string(), "bold".to_string()),
            ]
        );
    }

    // --- @import extraction -----------------------------------------------------------------

    #[test]
    fn extract_imports_manifest_forms() {
        let css = r#"
            @import "tokens.css";
            @import 'icons.css';
            @import url("carbon.css");
            @import url('../components/tooltip/tooltip.css');
            @import url(../../src/vue/components/feature/feature.css);
            @import "print.css" print;
            @import "responsive.css" screen and (min-width: 600px);
            .a { color: red }
        "#;
        let imports = extract_imports(css);
        assert_eq!(
            imports,
            vec![
                "tokens.css".to_string(),
                "icons.css".to_string(),
                "carbon.css".to_string(),
                "../components/tooltip/tooltip.css".to_string(),
                "../../src/vue/components/feature/feature.css".to_string(),
                "print.css".to_string(),
                "responsive.css".to_string(),
            ]
        );
    }

    #[test]
    fn extract_imports_ignores_nested_imports() {
        // An `@import` inside a block is not a real top-level import and must be ignored.
        let css = "@import \"top.css\"; @media screen { @import \"inner.css\"; } .a {}";
        assert_eq!(extract_imports(css), vec!["top.css".to_string()]);
    }

    #[test]
    fn extract_imports_none_when_absent() {
        assert!(extract_imports("p { color: red } .a { color: blue }").is_empty());
    }

    // --- CSS nesting ------------------------------------------------------------------------

    #[test]
    fn nesting_amp_pseudo() {
        let sheet = parse(".a { color: red; &:hover { color: blue } }");
        assert_eq!(sheet.rules.len(), 2);
        assert_eq!(sheet.rules[0].selectors, vec![".a"]);
        assert_eq!(sheet.rules[0].declarations, vec![("color".to_string(), "red".to_string())]);
        assert_eq!(sheet.rules[1].selectors, vec![".a:hover"]);
        assert_eq!(sheet.rules[1].declarations, vec![("color".to_string(), "blue".to_string())]);
    }

    #[test]
    fn nesting_bare_descendant() {
        let sheet = parse(".a { .b { color: green } }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors, vec![".a .b"]);
        assert_eq!(sheet.rules[0].declarations, vec![("color".to_string(), "green".to_string())]);
    }

    #[test]
    fn nesting_comma_expansion() {
        let sheet = parse(".a, .b { & .c { color: red } }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors, vec![".a .c", ".b .c"]);
    }

    #[test]
    fn nesting_recursive() {
        let sheet = parse(".a { color: red; & .b { color: green; &:hover { color: blue } } }");
        // .a {color:red}, .a .b {color:green}, .a .b:hover {color:blue}
        assert_eq!(sheet.rules.len(), 3);
        assert_eq!(sheet.rules[0].selectors, vec![".a"]);
        assert_eq!(sheet.rules[1].selectors, vec![".a .b"]);
        assert_eq!(sheet.rules[2].selectors, vec![".a .b:hover"]);
    }

    #[test]
    fn nesting_media_inside_rule_flattens() {
        let sheet = parse(".a { color: red; @media (min-width: 600px) { color: blue } }");
        assert_eq!(sheet.rules.len(), 2);
        assert_eq!(sheet.rules[0].selectors, vec![".a"]);
        assert_eq!(sheet.rules[0].media, None);
        assert_eq!(sheet.rules[1].selectors, vec![".a"]);
        assert_eq!(sheet.rules[1].media.as_deref(), Some("(min-width: 600px)"));
        assert_eq!(sheet.rules[1].declarations, vec![("color".to_string(), "blue".to_string())]);
    }

    #[test]
    fn non_nested_sheet_unchanged() {
        // Flat CSS must behave identically to before.
        let sheet = parse("h1 { color: red; font-size: 32px } p, .note { color: #00f }");
        assert_eq!(sheet.rules.len(), 2);
        assert_eq!(sheet.rules[0].selectors, vec!["h1"]);
        assert_eq!(
            sheet.rules[0].declarations,
            vec![
                ("color".to_string(), "red".to_string()),
                ("font-size".to_string(), "32px".to_string())
            ]
        );
        assert_eq!(sheet.rules[1].selectors, vec!["p", ".note"]);
    }

    #[test]
    fn malformed_input_does_not_panic() {
        let _ = parse("p { color: red");
        let _ = parse("}}} @media {{{ ");
        let _ = parse("/* unterminated");
        let _ = parse("{ no selector }");
        let _ = parse("@font-face");
    }
}
