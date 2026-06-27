#[derive(Clone, PartialEq, Debug)]
pub(crate) enum AttrMatchOp {
    Exists,
    Equals,
    Includes,
    DashMatch,
    Prefix,
    Suffix,
    Substring,
}

/// A parsed `[attr]` / `[attr op value]` condition for the `querySelector` selector engine.
#[derive(Clone, Debug)]
pub(crate) struct AttrCond {
    name: String, // local name, lowercased
    op: AttrMatchOp,
    value: String,
    ci: bool, // case-insensitive value match (the `i` flag)
}

/// A single compound selector, e.g. `div.foo#bar[disabled]`.
#[derive(Debug, Default, Clone)]
pub(crate) struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attrs: Vec<AttrCond>,
    any: bool,
}

impl Compound {
    fn matches(&self, doc: &dom::Document, node: dom::NodeId) -> bool {
        let e = match &doc.get(node).data {
            dom::NodeData::Element(e) => e,
            _ => return false,
        };
        if let Some(tag) = &self.tag {
            if tag != "*" && !e.tag.eq_ignore_ascii_case(tag) {
                return false;
            }
        }
        if let Some(id) = &self.id {
            if e.id() != Some(id.as_str()) {
                return false;
            }
        }
        for c in &self.classes {
            if !e.classes().any(|x| x == c) {
                return false;
            }
        }
        for a in &self.attrs {
            if !attr_cond_matches(e, a) {
                return false;
            }
        }
        true
    }
}

/// Strip a CSS attribute-namespace prefix (`*|`, `|`, or `ns|`) to the local name — our HTML
/// attributes carry no namespace, so any of these reduce to the local name.
pub(crate) fn strip_attr_ns(name: &str) -> &str {
    if let Some(rest) = name.strip_prefix("*|") {
        rest
    } else if let Some(rest) = name.strip_prefix('|') {
        rest
    } else if let Some(bar) = name.find('|') {
        &name[bar + 1..]
    } else {
        name
    }
}

/// Parse the inside of an attribute selector `[...]` into an [`AttrCond`].
pub(crate) fn parse_attr_cond(inner: &str) -> Option<AttrCond> {
    let s = inner.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(eq) = s.find('=') {
        // The operator is `=` optionally prefixed by one of ~ | ^ $ * (immediately before it).
        let prev = if eq > 0 {
            s.as_bytes().get(eq - 1).copied()
        } else {
            None
        };
        let (op, name_end) = match prev {
            Some(b'~') => (AttrMatchOp::Includes, eq - 1),
            Some(b'|') => (AttrMatchOp::DashMatch, eq - 1),
            Some(b'^') => (AttrMatchOp::Prefix, eq - 1),
            Some(b'$') => (AttrMatchOp::Suffix, eq - 1),
            Some(b'*') => (AttrMatchOp::Substring, eq - 1),
            _ => (AttrMatchOp::Equals, eq),
        };
        let name = s[..name_end].trim();
        if name.is_empty() {
            return None;
        }
        let mut raw_val = s[eq + 1..].trim();
        // Optional trailing case-sensitivity flag (whitespace-separated `i`/`s`).
        let mut ci = false;
        if let Some(v) = raw_val
            .strip_suffix(" i")
            .or_else(|| raw_val.strip_suffix(" I"))
        {
            raw_val = v.trim_end();
            ci = true;
        } else if let Some(v) = raw_val
            .strip_suffix(" s")
            .or_else(|| raw_val.strip_suffix(" S"))
        {
            raw_val = v.trim_end();
        }
        let value = unquote_attr_value(raw_val);
        Some(AttrCond {
            name: strip_attr_ns(name).to_ascii_lowercase(),
            op,
            value,
            ci,
        })
    } else {
        Some(AttrCond {
            name: strip_attr_ns(s).to_ascii_lowercase(),
            op: AttrMatchOp::Exists,
            value: String::new(),
            ci: false,
        })
    }
}

/// Strip matching surrounding single/double quotes from an attribute-selector value.
pub(crate) fn unquote_attr_value(s: &str) -> String {
    let s = s.trim();
    let b = s.as_bytes();
    if s.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Match an [`AttrCond`] against an element (mirrors the cascade's attribute matching).
pub(crate) fn attr_cond_matches(e: &dom::ElementData, a: &AttrCond) -> bool {
    let actual = e
        .attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(&a.name))
        .map(|(_, v)| v.as_str());
    let Some(val) = actual else {
        return false;
    };
    if a.op == AttrMatchOp::Exists {
        return true;
    }
    let (hay, needle) = if a.ci {
        (val.to_ascii_lowercase(), a.value.to_ascii_lowercase())
    } else {
        (val.to_string(), a.value.clone())
    };
    match a.op {
        AttrMatchOp::Exists => true,
        AttrMatchOp::Equals => hay == needle,
        AttrMatchOp::Includes => !needle.is_empty() && hay.split_whitespace().any(|w| w == needle),
        AttrMatchOp::DashMatch => hay == needle || hay.starts_with(&format!("{needle}-")),
        AttrMatchOp::Prefix => !needle.is_empty() && hay.starts_with(&needle),
        AttrMatchOp::Suffix => !needle.is_empty() && hay.ends_with(&needle),
        AttrMatchOp::Substring => !needle.is_empty() && hay.contains(&needle),
    }
}

/// Is `c` a CSS hex digit?
pub(crate) fn is_hex(c: char) -> bool {
    c.is_ascii_hexdigit()
}

/// Read a CSS identifier (class / id / type name) starting at `i`, consuming CSS escape sequences
/// (`\` + 1-6 hex digits with optional trailing whitespace, or `\` + any other char as a literal).
/// Stops at an *unescaped* selector delimiter (`.` `#` `[` `:` `>` `+` `~` `,` ` `). Returns the
/// unescaped value and the index just past the identifier.
pub(crate) fn read_css_ident(bytes: &[char], mut i: usize) -> (String, usize) {
    let mut out = String::new();
    while i < bytes.len() {
        let ch = bytes[i];
        if ch == '\\' {
            // Escape sequence.
            i += 1;
            if i >= bytes.len() {
                break;
            }
            if is_hex(bytes[i]) {
                // Up to 6 hex digits, then an optional single whitespace.
                let mut hex = String::new();
                let mut k = 0;
                while i < bytes.len() && k < 6 && is_hex(bytes[i]) {
                    hex.push(bytes[i]);
                    i += 1;
                    k += 1;
                }
                if i < bytes.len() && matches!(bytes[i], ' ' | '\t' | '\n' | '\r' | '\u{0C}') {
                    i += 1; // consume one trailing whitespace
                }
                if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                    // Per CSS: a NULL, an out-of-range, or a surrogate codepoint => U+FFFD.
                    let ch = if cp == 0 || cp > 0x10FFFF || (0xD800..=0xDFFF).contains(&cp) {
                        '\u{FFFD}'
                    } else {
                        char::from_u32(cp).unwrap_or('\u{FFFD}')
                    };
                    out.push(ch);
                }
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        } else if matches!(ch, '.' | '#' | '[' | ':' | '>' | '+' | '~' | ',') || ch.is_whitespace()
        {
            break;
        } else {
            out.push(ch);
            i += 1;
        }
    }
    (out, i)
}

/// Parse a single compound selector (no combinators).
pub(crate) fn parse_compound(s: &str) -> Option<Compound> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut c = Compound::default();
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        match ch {
            '.' | '#' => {
                i += 1;
                let (name, ni) = read_css_ident(&bytes, i);
                i = ni;
                if name.is_empty() {
                    return None;
                }
                if ch == '.' {
                    c.classes.push(name);
                } else {
                    c.id = Some(name);
                }
                c.any = true;
            }
            '[' => {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != ']' {
                    i += 1;
                }
                let inner: String = bytes[start..i].iter().collect();
                if i < bytes.len() {
                    i += 1; // consume ']'
                }
                if let Some(cond) = parse_attr_cond(&inner) {
                    c.attrs.push(cond);
                }
                c.any = true;
            }
            ':' => {
                i += 1;
                if i < bytes.len() && bytes[i] == ':' {
                    i += 1;
                }
                while i < bytes.len() && !matches!(bytes[i], '.' | '#' | '[' | ':') {
                    if bytes[i] == '(' {
                        let mut depth = 1;
                        i += 1;
                        while i < bytes.len() && depth > 0 {
                            match bytes[i] {
                                '(' => depth += 1,
                                ')' => depth -= 1,
                                _ => {}
                            }
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
                c.any = true;
            }
            _ => {
                let (tag, ni) = read_css_ident(&bytes, i);
                if ni == i {
                    // Not a valid identifier start (e.g. stray char); skip it to avoid a loop.
                    i += 1;
                    continue;
                }
                i = ni;
                // read_css_ident swallows the namespace prefix (`*` and `|` aren't ident stops), so a
                // type selector arrives as e.g. `*|body` / `svg|rect` / `|div`. Our DOM carries no
                // element namespaces, so reduce any namespace prefix to the local name (matching the
                // attribute-namespace handling); `*|*` collapses to the universal `*`.
                let tag = strip_attr_ns(tag.trim()).to_string();
                if !tag.is_empty() {
                    c.tag = Some(tag);
                    c.any = true;
                }
            }
        }
    }
    if c.any {
        Some(c)
    } else {
        None
    }
}

/// A complex selector: a chain of compounds joined by descendant combinators (whitespace).
/// Splitting is escape-aware so a CSS escape that contains whitespace (`#\30 foo`) or a combinator
/// character stays within its compound; only *unescaped* combinators/whitespace separate compounds.
/// A CSS combinator between two compound selectors.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Combinator {
    Descendant,        // `A B`
    Child,             // `A > B`
    NextSibling,       // `A + B`
    SubsequentSibling, // `A ~ B`
}

/// The previous element sibling of `node` (skipping text / comment nodes), if any.
pub(crate) fn prev_element_sibling(doc: &dom::Document, node: dom::NodeId) -> Option<dom::NodeId> {
    let parent = doc.get(node).parent?;
    let siblings = &doc.get(parent).children;
    let pos = siblings.iter().position(|&s| s == node)?;
    siblings[..pos]
        .iter()
        .rev()
        .find(|&&s| matches!(doc.get(s).data, dom::NodeData::Element(_)))
        .copied()
}

/// Parse a complex selector into `(combinator-to-previous, compound)` pairs in source order. The
/// first pair's combinator is `Descendant` (unused — it has no left neighbor).
pub(crate) fn parse_complex(s: &str) -> Option<Vec<(Combinator, Compound)>> {
    let bytes: Vec<char> = s.chars().collect();
    let mut segments: Vec<(Combinator, String)> = Vec::new();
    let mut cur = String::new();
    let mut pending = Combinator::Descendant; // combinator preceding the next segment
    let mut i = 0;
    let mut bracket_depth = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        if ch == '\\' {
            // Keep the whole escape verbatim for parse_compound to unescape. A hex escape is
            // backslash + 1-6 hex digits + an optional single trailing whitespace; that trailing
            // whitespace must NOT be treated as a descendant combinator here.
            cur.push(ch);
            i += 1;
            if i < bytes.len() && is_hex(bytes[i]) {
                let mut k = 0;
                while i < bytes.len() && k < 6 && is_hex(bytes[i]) {
                    cur.push(bytes[i]);
                    i += 1;
                    k += 1;
                }
                if i < bytes.len() && matches!(bytes[i], ' ' | '\t' | '\n' | '\r' | '\u{0C}') {
                    cur.push(bytes[i]);
                    i += 1;
                }
            } else if i < bytes.len() {
                cur.push(bytes[i]);
                i += 1;
            }
            continue;
        }
        if ch == '[' {
            bracket_depth += 1;
        } else if ch == ']' && bracket_depth > 0 {
            bracket_depth -= 1;
        }
        if bracket_depth == 0 && (matches!(ch, '>' | '+' | '~') || ch.is_whitespace()) {
            if !cur.trim().is_empty() {
                segments.push((pending, std::mem::take(&mut cur)));
                pending = Combinator::Descendant;
            } else {
                cur.clear();
            }
            match ch {
                '>' => pending = Combinator::Child,
                '+' => pending = Combinator::NextSibling,
                '~' => pending = Combinator::SubsequentSibling,
                _ => {} // whitespace → descendant (unless an explicit combinator follows)
            }
            i += 1;
            continue;
        }
        cur.push(ch);
        i += 1;
    }
    if !cur.trim().is_empty() {
        segments.push((pending, cur));
    }
    let parts: Vec<(Combinator, Compound)> = segments
        .iter()
        .filter_map(|(c, s)| parse_compound(s).map(|cp| (*c, cp)))
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

/// Does `node` match the complex selector `chain` (matched right-to-left, with backtracking for the
/// descendant and subsequent-sibling combinators)?
pub(crate) fn matches_complex(
    doc: &dom::Document,
    node: dom::NodeId,
    chain: &[(Combinator, Compound)],
) -> bool {
    let n = chain.len();
    if n == 0 {
        return false;
    }
    if !chain[n - 1].1.matches(doc, node) {
        return false;
    }
    if n == 1 {
        return true;
    }
    // `chain[n-1].0` links `chain[n-2]` (left) to `chain[n-1]` (which matched `node`).
    let rest = &chain[..n - 1];
    match chain[n - 1].0 {
        Combinator::Child => match doc.get(node).parent {
            Some(p) => matches_complex(doc, p, rest),
            None => false,
        },
        Combinator::NextSibling => match prev_element_sibling(doc, node) {
            Some(prev) => matches_complex(doc, prev, rest),
            None => false,
        },
        Combinator::Descendant => {
            let mut cur = doc.get(node).parent;
            while let Some(p) = cur {
                if matches_complex(doc, p, rest) {
                    return true;
                }
                cur = doc.get(p).parent;
            }
            false
        }
        Combinator::SubsequentSibling => {
            let mut cur = prev_element_sibling(doc, node);
            while let Some(s) = cur {
                if matches_complex(doc, s, rest) {
                    return true;
                }
                cur = prev_element_sibling(doc, s);
            }
            false
        }
    }
}

/// Collect every node matching any of the comma-separated selector groups, document order.
pub(crate) fn query_selector_all(doc: &dom::Document, sel: &str) -> Vec<dom::NodeId> {
    let groups: Vec<Vec<(Combinator, Compound)>> =
        sel.split(',').filter_map(parse_complex).collect();
    if groups.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    fn walk(
        doc: &dom::Document,
        node: dom::NodeId,
        groups: &[Vec<(Combinator, Compound)>],
        out: &mut Vec<dom::NodeId>,
    ) {
        if matches!(doc.get(node).data, dom::NodeData::Element(_))
            && groups.iter().any(|g| matches_complex(doc, node, g))
        {
            out.push(node);
        }
        let children = doc.get(node).children.clone();
        for child in children {
            walk(doc, child, groups, out);
        }
    }
    walk(doc, doc.root(), &groups, &mut out);
    out
}

/// Like [`query_selector_all`] but scoped to the subtree under `root` (excluding `root` itself).
pub(crate) fn query_within(doc: &dom::Document, root: dom::NodeId, sel: &str) -> Vec<dom::NodeId> {
    let groups: Vec<Vec<(Combinator, Compound)>> =
        sel.split(',').filter_map(parse_complex).collect();
    let mut out = Vec::new();
    if groups.is_empty() {
        return out;
    }
    fn walk(
        doc: &dom::Document,
        node: dom::NodeId,
        groups: &[Vec<(Combinator, Compound)>],
        out: &mut Vec<dom::NodeId>,
    ) {
        if matches!(doc.get(node).data, dom::NodeData::Element(_))
            && groups.iter().any(|g| matches_complex(doc, node, g))
        {
            out.push(node);
        }
        let children = doc.get(node).children.clone();
        for child in children {
            walk(doc, child, groups, out);
        }
    }
    let children = doc.get(root).children.clone();
    for child in children {
        walk(doc, child, &groups, &mut out);
    }
    out
}

/// Collect every element under `root` carrying ALL of `wanted` classes, document order.
pub(crate) fn collect_by_class(
    doc: &dom::Document,
    root: dom::NodeId,
    wanted: &[String],
    out: &mut Vec<dom::NodeId>,
) {
    if let dom::NodeData::Element(e) = &doc.get(root).data {
        if !wanted.is_empty() && wanted.iter().all(|w| e.classes().any(|c| c == w)) {
            out.push(root);
        }
    }
    let children = doc.get(root).children.clone();
    for child in children {
        collect_by_class(doc, child, wanted, out);
    }
}

// ---------------------------------------------------------------------------------------------
// V8 value conversion helpers.
// ---------------------------------------------------------------------------------------------
