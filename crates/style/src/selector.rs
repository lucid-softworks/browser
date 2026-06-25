use crate::*;

/// If any selector in `selectors` matches `el`, return the highest specificity among the
/// matching ones (encoded as id*100 + class*10 + type). `None` if none match.
///
/// The cascade now matches via [`SelectorIndex`] rather than calling this per rule, but it is
/// retained as the reference single-rule matcher (used by tests / external callers).
#[allow(dead_code)]
pub(crate) fn rule_specificity(
    selectors: &[String],
    doc: &dom::Document,
    id: dom::NodeId,
) -> Option<u32> {
    let mut best: Option<u32> = None;
    for sel in selectors {
        if let Some(c) = compile_selector(sel) {
            if complex_matches(doc, id, &c.selector) {
                best = Some(best.map_or(c.specificity, |b| b.max(c.specificity)));
            }
        }
    }
    best
}

// ===========================================================================================
// Complex selector engine
// ===========================================================================================
//
// A *complex* selector is a sequence of compound selectors joined by combinators, evaluated
// right-to-left. We parse each selector string into a [`ComplexSelector`] (a `Vec` of
// `(Combinator, Compound)` stored RIGHTMOST-FIRST) and match it against a `(doc, node_id)`
// pair by walking the appropriate tree axis for each combinator, with backtracking for the
// descendant / general-sibling axes.
//
// Pseudo-ELEMENTS (`::before`, `::after`, `::placeholder`, `::marker`) are OUT OF SCOPE: a
// selector containing one is treated as non-matching (its parse returns `None`, so the rule is
// dropped from the index) — we never crash on it.

/// How a compound relates to the compound on its right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Combinator {
    /// Rightmost compound (no left neighbor) — the "subject" of the selector.
    Subject,
    /// Descendant (whitespace): some ancestor matches.
    Descendant,
    /// Child (`>`): the parent matches.
    Child,
    /// Adjacent sibling (`+`): the immediately-preceding element sibling matches.
    NextSibling,
    /// General sibling (`~`): some preceding element sibling matches.
    SubsequentSibling,
}

/// One attribute selector `[name OP value FLAG]`.
#[derive(Debug, Clone)]
pub(crate) struct AttrSel {
    name: String,
    op: AttrOp,
    value: String,
    case_insensitive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttrOp {
    /// `[attr]` — present.
    Exists,
    /// `[attr=val]`.
    Equals,
    /// `[attr~=val]` — whitespace-separated word.
    Includes,
    /// `[attr|=val]` — val or val-`-`….
    DashMatch,
    /// `[attr^=val]` — prefix.
    Prefix,
    /// `[attr$=val]` — suffix.
    Suffix,
    /// `[attr*=val]` — substring.
    Substring,
}

/// A pseudo-class. Structural ones need tree/sibling position; state ones consult interaction
/// state or element attributes; functional ones recurse into nested selector lists.
#[derive(Debug, Clone)]
pub(crate) enum Pseudo {
    // Structural
    FirstChild,
    LastChild,
    OnlyChild,
    FirstOfType,
    LastOfType,
    OnlyOfType,
    NthChild(NthArg),
    NthLastChild(NthArg),
    NthOfType(NthArg),
    NthLastOfType(NthArg),
    Root,
    Empty,
    // State (attribute-derived)
    Checked,
    Disabled,
    Enabled,
    Required,
    Optional,
    Link, // <a href> (also :any-link)
    /// `:visited` — a link to a page in history. We treat a link to the current page (empty or
    /// pure-fragment href) as visited (history has no other entries in our model).
    Visited,
    // State (interaction)
    Hover,
    Focus,
    Active,
    FocusWithin,
    FocusVisible,
    /// `:lang(<ident>)` — matches if the element's (inherited) `lang` equals the argument or begins
    /// with `<arg>-`. The argument is stored lowercased.
    Lang(String),
    // Functional
    Not(Vec<ComplexSelector>),
    Is(Vec<ComplexSelector>),
    Where(Vec<ComplexSelector>),
    /// Recognized-but-never-matches (`:visited`, `:target`, `:default`, `:placeholder-shown`,
    /// etc.) — best-effort: parses cleanly so the rest of the selector still works, but the
    /// element never matches it.
    NeverMatch,
}

/// An `An+B` argument for `:nth-*`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NthArg {
    a: i32,
    b: i32,
}

impl NthArg {
    /// Does a 1-based index `n` satisfy `An+B`? i.e. exists k>=0 with n == a*k + b.
    fn matches(&self, n: i32) -> bool {
        if self.a == 0 {
            return n == self.b;
        }
        let diff = n - self.b;
        diff % self.a == 0 && diff / self.a >= 0
    }
}

/// The namespace component of a type/universal selector (`svg|`, `*|`, `|`, or none).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum NsConstraint {
    /// No namespace prefix written. With a declared default `@namespace`, the type is constrained to
    /// it; otherwise (no default) it matches any namespace.
    #[default]
    Unspecified,
    /// `*|` — explicitly any namespace.
    Any,
    /// `|` — explicitly the null namespace (no namespace).
    None,
    /// `prefix|` — the namespace bound to `prefix` by an `@namespace` rule.
    Prefixed(String),
}

/// A compound selector: an optional type plus any number of class/id/attr/pseudo simples.
#[derive(Debug, Clone, Default)]
pub(crate) struct Compound {
    /// Leading type, lowercased. `None` = universal (`*`) or no explicit type.
    type_part: Option<String>,
    /// The namespace component of the (possibly implied universal) type selector.
    type_ns: NsConstraint,
    classes: Vec<String>,
    ids: Vec<String>,
    attrs: Vec<AttrSel>,
    pseudos: Vec<Pseudo>,
}

/// A full complex selector, stored rightmost-compound-first. `parts[0]` is the subject.
#[derive(Debug, Clone)]
pub(crate) struct ComplexSelector {
    parts: Vec<(Combinator, Compound)>,
    specificity: u32,
    /// A trailing `::before`/`::after` (or legacy `:before`/`:after`) on the subject compound.
    /// `None` for an ordinary element selector.
    pseudo_element: Option<PseudoElement>,
}

/// What we bucket a compiled selector under in the [`SelectorIndex`]: the most-selective simple
/// part of the RIGHTMOST (subject) compound.
#[derive(Debug, Clone)]
pub(crate) enum BucketKey {
    Id(String),
    Class(String),
    Type(String),
    Universal,
}

/// A CSS pseudo-element. `Before`/`After` generate content boxes during layout; everything else is
/// modeled as `Other(key)` so author rules targeting it can still match (for `getComputedStyle`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PseudoElement {
    Before,
    After,
    /// Any other pseudo-element (`::marker`, `::placeholder`, `::highlight(name)`,
    /// `::picker(select)`, …), stored as its normalized key (lowercased name, plus a normalized
    /// `(arg)` for functional pseudos). These don't generate layout boxes here, but they DO match
    /// author rules so `getComputedStyle(el, "::marker")` can return the pseudo's cascaded style.
    Other(String),
}

impl PseudoElement {
    /// The canonical key two selectors / a getComputedStyle arg compare equal on.
    pub fn key(&self) -> String {
        match self {
            PseudoElement::Before => "before".to_string(),
            PseudoElement::After => "after".to_string(),
            PseudoElement::Other(k) => k.clone(),
        }
    }
}

/// A compiled selector ready for the index: the parsed [`ComplexSelector`] plus its bucket key.
#[derive(Debug, Clone)]
pub(crate) struct Compiled {
    pub(crate) selector: ComplexSelector,
    pub(crate) key: BucketKey,
    pub(crate) specificity: u32,
    /// `Some` if this selector targets a `::before`/`::after` pseudo-element. The rest of the
    /// selector still matches the ORIGINATING element normally; this just routes the result.
    pub(crate) pseudo_element: Option<PseudoElement>,
}

impl Compiled {
    pub(crate) fn bucket_key(&self) -> &BucketKey {
        &self.key
    }
}

/// Specificity weights packed into a sortable u32 (a*10000 + b*100 + c), matching the existing
/// scheme's magnitude (id=100, class=10, type=1 historically; the new packing keeps the same
/// relative ordering with more headroom for many components).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Spec {
    a: u32, // ids
    b: u32, // classes / attrs / pseudo-classes
    c: u32, // types / pseudo-elements
}

impl Spec {
    fn pack(&self) -> u32 {
        self.a.min(9999) * 10000 + self.b.min(99) * 100 + self.c.min(99)
    }
    fn add(&mut self, o: Spec) {
        self.a += o.a;
        self.b += o.b;
        self.c += o.c;
    }
    fn max_with(self, o: Spec) -> Spec {
        if (self.a, self.b, self.c) >= (o.a, o.b, o.c) {
            self
        } else {
            o
        }
    }
}

/// Parse one (possibly complex) selector string into a [`Compiled`], or `None` if it uses
/// syntax we never match — chiefly pseudo-ELEMENTS (`::before`) or malformed input. This is the
/// single source of truth for selector parsing used by the cascade index.
pub(crate) fn compile_selector(sel: &str) -> Option<Compiled> {
    let selector = parse_complex(sel)?;
    let specificity = selector.specificity;
    // Bucket key = most-selective simple part of the rightmost (subject) compound.
    let subject = &selector.parts[0].1;
    let key = if let Some(id) = subject.ids.first() {
        BucketKey::Id(id.clone())
    } else if let Some(class) = subject.classes.first() {
        BucketKey::Class(class.clone())
    } else if let Some(t) = &subject.type_part {
        BucketKey::Type(t.clone())
    } else {
        // Purely `[attr]`/`:pseudo`/`*` subject → universal bucket.
        BucketKey::Universal
    };
    let pseudo_element = selector.pseudo_element.clone();
    Some(Compiled {
        selector,
        key,
        specificity,
        pseudo_element,
    })
}

/// Parse a complex selector into rightmost-first `(Combinator, Compound)` parts, computing its
/// specificity. Returns `None` if any compound fails to parse (e.g. a pseudo-element).
pub(crate) fn parse_complex(sel: &str) -> Option<ComplexSelector> {
    let chars: Vec<char> = sel.trim().chars().collect();
    if chars.is_empty() {
        return None;
    }
    // Tokenize into (combinator-to-the-left, compound-text) pairs, left-to-right, then reverse.
    // We split on top-level whitespace / `>` / `+` / `~` (not inside [], (), or quotes).
    let mut parts: Vec<(Combinator, String)> = Vec::new();
    let mut cur = String::new();
    // Combinator that precedes the NEXT compound to be flushed (relates it to the PREVIOUS
    // compound). The first compound has no preceding combinator (`Subject`).
    let mut pending_comb = Combinator::Subject;
    let mut i = 0;
    let mut depth_brk = 0i32; // []
    let mut depth_par = 0i32; // ()
    let mut quote: Option<char> = None;
    let n = chars.len();
    // Flush the current compound text, tagged with the pending combinator; reset pending to the
    // "no combinator seen yet" sentinel for the next compound.
    let flush =
        |cur: &mut String, pending: &mut Combinator, parts: &mut Vec<(Combinator, String)>| {
            if !cur.is_empty() {
                parts.push((*pending, std::mem::take(cur)));
                *pending = Combinator::Subject; // sentinel; overwritten before next flush
            }
        };
    while i < n {
        let c = chars[i];
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            '[' => {
                depth_brk += 1;
                cur.push(c);
            }
            ']' => {
                depth_brk -= 1;
                cur.push(c);
            }
            '(' => {
                depth_par += 1;
                cur.push(c);
            }
            ')' => {
                depth_par -= 1;
                cur.push(c);
            }
            _ if depth_brk > 0 || depth_par > 0 => cur.push(c),
            c if c.is_whitespace() => {
                // Whitespace: flush the current compound, then tentatively mark the next
                // combinator as descendant. An explicit combinator immediately after overrides
                // it (so `.a > .b` parses Child, not Descendant).
                flush(&mut cur, &mut pending_comb, &mut parts);
                pending_comb = Combinator::Descendant;
                while i + 1 < n && chars[i + 1].is_whitespace() {
                    i += 1;
                }
            }
            '>' | '+' | '~' => {
                // Explicit combinator relates the NEXT compound to the previous one. Flush the
                // current compound first (handles the no-whitespace case `a>b`).
                flush(&mut cur, &mut pending_comb, &mut parts);
                pending_comb = match c {
                    '>' => Combinator::Child,
                    '+' => Combinator::NextSibling,
                    _ => Combinator::SubsequentSibling,
                };
                while i + 1 < n && chars[i + 1].is_whitespace() {
                    i += 1;
                }
            }
            _ => cur.push(c),
        }
        i += 1;
    }
    flush(&mut cur, &mut pending_comb, &mut parts);
    if parts.is_empty() {
        return None;
    }
    // `parts[i].0` is the combinator BEFORE compound i (linking it to compound i-1) in source
    // order. We want each compound to carry the combinator linking it to its RIGHT neighbor:
    //   right_link[i] = before[i+1]   (and the last compound's right_link = Subject).
    // Then reversing puts the subject at index 0, and every `match_from` step at index `idx`
    // reads the combinator relating `parts[idx]` to `parts[idx-1]` (its source-right neighbor).
    let k = parts.len();
    let mut right_link: Vec<Combinator> = Vec::with_capacity(k);
    for i in 0..k {
        if i + 1 < k {
            right_link.push(parts[i + 1].0);
        } else {
            right_link.push(Combinator::Subject);
        }
    }

    let mut out: Vec<(Combinator, Compound)> = Vec::with_capacity(k);
    let mut spec = Spec::default();
    let mut pseudo_element = None;
    // Build rightmost-first. Only the rightmost (subject, source-last) compound may carry a
    // trailing `::before`/`::after`.
    for i in (0..k).rev() {
        let is_subject = i == k - 1;
        let (compound, cspec, pe) = parse_compound(&parts[i].1)?;
        // A pseudo-element is only valid on the subject; anywhere else it's malformed.
        if pe.is_some() && !is_subject {
            return None;
        }
        if is_subject {
            pseudo_element = pe;
        }
        spec.add(cspec);
        out.push((right_link[i], compound));
    }
    Some(ComplexSelector {
        parts: out,
        specificity: spec.pack(),
        pseudo_element,
    })
}

/// Parse a single compound selector (`type.class#id[attr]:pseudo`...). Returns the compound and
/// its specificity, or `None` on a pseudo-element / malformed token.
pub(crate) fn parse_compound(text: &str) -> Option<(Compound, Spec, Option<PseudoElement>)> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut compound = Compound::default();
    let mut spec = Spec::default();
    // Set when we strip a trailing `::before`/`::after` (or legacy single-colon). The pseudo-element
    // must be the LAST token in the compound, so once seen, nothing else may follow.
    let mut pseudo_element: Option<PseudoElement> = None;

    // Optional leading namespace prefix + type / universal. A `|` may appear after an ident, a `*`,
    // or at the very start (`|tag` = null namespace). Recognize: `prefix|tag`, `prefix|*`, `*|tag`,
    // `*|*`, `|tag`, `|*`, plain `tag`, plain `*`.
    // First, read an optional prefix token (`ident`, `*`, or empty) that is immediately followed by
    // a `|` (but not `||`, the column combinator).
    let has_ns_pipe = {
        // Scan a tentative prefix: empty, `*`, or an ident, then require a single `|` not followed
        // by another `|`.
        let mut k = i;
        if k < n && chars[k] == '*' {
            k += 1;
        } else {
            while k < n && (chars[k].is_alphanumeric() || chars[k] == '-' || chars[k] == '_') {
                k += 1;
            }
        }
        k < n && chars[k] == '|' && !(k + 1 < n && chars[k + 1] == '|')
    };
    if has_ns_pipe {
        let pstart = i;
        if i < n && chars[i] == '*' {
            i += 1;
            compound.type_ns = NsConstraint::Any;
        } else {
            while i < n && (chars[i].is_alphanumeric() || chars[i] == '-' || chars[i] == '_') {
                i += 1;
            }
            let prefix: String = chars[pstart..i].iter().collect();
            compound.type_ns = if prefix.is_empty() {
                NsConstraint::None
            } else {
                NsConstraint::Prefixed(prefix)
            };
        }
        i += 1; // consume the `|`
                // Now read the type / universal that follows the namespace prefix.
        if i < n && chars[i] == '*' {
            i += 1; // universal within this namespace
        } else if i < n && !matches!(chars[i], '.' | '#' | '[' | ':') {
            let start = i;
            while i < n && !matches!(chars[i], '.' | '#' | '[' | ':' | '*') {
                i += 1;
            }
            let t: String = chars[start..i].iter().collect();
            if !is_ident(&t) {
                return None;
            }
            compound.type_part = Some(t.to_lowercase());
            spec.c += 1;
        } else {
            return None; // `ns|` with no type/universal is invalid
        }
    } else if i < n
        && chars[i] != '.'
        && chars[i] != '#'
        && chars[i] != '['
        && chars[i] != ':'
        && chars[i] != '*'
    {
        let start = i;
        while i < n && !matches!(chars[i], '.' | '#' | '[' | ':' | '*') {
            i += 1;
        }
        let t: String = chars[start..i].iter().collect();
        if !is_ident(&t) {
            return None;
        }
        compound.type_part = Some(t.to_lowercase());
        spec.c += 1;
    } else if i < n && chars[i] == '*' {
        i += 1; // universal, no specificity, no type constraint
    }

    while i < n {
        match chars[i] {
            '.' => {
                i += 1;
                let start = i;
                while i < n && is_name_char(chars[i]) {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                if name.is_empty() {
                    return None;
                }
                compound.classes.push(name);
                spec.b += 1;
            }
            '#' => {
                i += 1;
                let start = i;
                while i < n && is_name_char(chars[i]) {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                if name.is_empty() {
                    return None;
                }
                compound.ids.push(name);
                spec.a += 1;
            }
            '[' => {
                // Read up to the matching ']' (no nested brackets in attribute selectors).
                let start = i + 1;
                let mut j = start;
                let mut quote: Option<char> = None;
                while j < n {
                    let c = chars[j];
                    if let Some(q) = quote {
                        if c == q {
                            quote = None;
                        }
                    } else if c == '"' || c == '\'' {
                        quote = Some(c);
                    } else if c == ']' {
                        break;
                    }
                    j += 1;
                }
                if j >= n {
                    return None; // unterminated
                }
                let inner: String = chars[start..j].iter().collect();
                let attr = parse_attr(&inner)?;
                compound.attrs.push(attr);
                spec.b += 1;
                i = j + 1;
            }
            ':' => {
                // A pseudo-element uses double-colon (`::before`) syntax; legacy CSS2 also allowed
                // single-colon (`:before`). Detect either and, for the two we support, strip them
                // (routing the result to the element's ::before/::after style); a pseudo-element
                // must be the rightmost token, so nothing may follow it.
                let double_colon = i + 1 < n && chars[i + 1] == ':';
                let name_start = if double_colon { i + 2 } else { i + 1 };
                let mut j = name_start;
                while j < n && is_name_char(chars[j]) {
                    j += 1;
                }
                let pe_name: String = chars[name_start..j].iter().collect();
                let pe_name_l = pe_name.to_ascii_lowercase();
                // A pseudo-element may be functional (`::highlight(name)`, `::picker(select)`); its
                // `(arg)` is part of the pseudo-element token.
                let mut after_pe = j;
                let pe_arg: Option<String> = if after_pe < n && chars[after_pe] == '(' {
                    let astart = after_pe + 1;
                    let mut depth = 1i32;
                    let mut k = astart;
                    while k < n && depth > 0 {
                        match chars[k] {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        k += 1;
                    }
                    if k >= n {
                        return None;
                    }
                    let a: String = chars[astart..k].iter().collect();
                    after_pe = k + 1;
                    Some(a)
                } else {
                    None
                };
                // `::before` / `:before` and `::after` / `:after` are the box-generating pseudos.
                // Single-colon is legacy CSS2 syntax, valid only for the four original pseudo-
                // elements; every other pseudo-element requires double-colon.
                let legacy_single = matches!(
                    pe_name_l.as_str(),
                    "before" | "after" | "first-line" | "first-letter"
                );
                let known_pe = if !double_colon && !legacy_single {
                    None
                } else {
                    match pe_name_l.as_str() {
                        "before" if pe_arg.is_none() => Some(PseudoElement::Before),
                        "after" if pe_arg.is_none() => Some(PseudoElement::After),
                        _ => pseudo_element_key(&pe_name_l, pe_arg.as_deref())
                            .map(PseudoElement::Other),
                    }
                };
                if let Some(pe) = known_pe {
                    // A pseudo-element must be the rightmost token in the compound.
                    if after_pe != n {
                        return None;
                    }
                    pseudo_element = Some(pe);
                    // A pseudo-element contributes one type-level (c) specificity unit.
                    spec.c += 1;
                    i = after_pe;
                    continue;
                }
                // Double-colon syntax is *only* for pseudo-elements; an unrecognized one is invalid.
                if double_colon {
                    return None;
                }
                i += 1;
                let start = i;
                while i < n && is_name_char(chars[i]) {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                let name_l = name.to_ascii_lowercase();
                // Functional pseudo with `(...)`.
                let arg = if i < n && chars[i] == '(' {
                    let astart = i + 1;
                    let mut depth = 1i32;
                    let mut j = astart;
                    let mut quote: Option<char> = None;
                    while j < n && depth > 0 {
                        let c = chars[j];
                        if let Some(q) = quote {
                            if c == q {
                                quote = None;
                            }
                        } else if c == '"' || c == '\'' {
                            quote = Some(c);
                        } else if c == '(' {
                            depth += 1;
                        } else if c == ')' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        j += 1;
                    }
                    if j >= n {
                        return None;
                    }
                    let a: String = chars[astart..j].iter().collect();
                    i = j + 1;
                    Some(a)
                } else {
                    None
                };
                let (pseudo, pspec) = parse_pseudo(&name_l, arg.as_deref())?;
                spec.add(pspec);
                compound.pseudos.push(pseudo);
            }
            '*' => return None, // universal not allowed mid-compound
            _ => return None,
        }
    }
    Some((compound, spec, pseudo_element))
}

/// Validate and normalize a pseudo-element `name` (already lowercased) plus its optional functional
/// `arg` into a canonical key. Returns `None` for unrecognized pseudo-elements or malformed args.
/// `before`/`after` are handled by the caller as their own enum variants; this covers the rest.
///
/// The key is the lowercased name, plus `(arg)` for functional pseudos where `arg` is the
/// normalized (unescaped, lowercased ident) argument. The accepted set mirrors the WPT corpus.
pub(crate) fn pseudo_element_key(name: &str, arg: Option<&str>) -> Option<String> {
    // Functional pseudo-elements: name -> validator for the argument.
    //   ::highlight(<ident>), ::view-transition-*(<ident>|*), ::picker(<ident>)
    let functional: &[&str] = &[
        "highlight",
        "view-transition-group",
        "view-transition-image-pair",
        "view-transition-old",
        "view-transition-new",
        "picker",
    ];
    // Tree-structural / plain pseudo-elements (no argument).
    let plain: &[&str] = &[
        "first-line",
        "first-letter",
        "marker",
        "placeholder",
        "selection",
        "backdrop",
        "file-selector-button",
        "grammar-error",
        "spelling-error",
        "target-text",
        "view-transition",
        "checkmark",
        "picker-icon",
    ];

    match arg {
        Some(raw) => {
            if !functional.contains(&name) {
                return None; // a non-functional pseudo got an argument → invalid
            }
            // The argument must be a single CSS identifier. Surrounding whitespace is allowed;
            // escapes are decoded. (`*` is NOT accepted for view-transition-* in getComputedStyle.)
            let trimmed = raw.trim_matches(|c: char| c.is_ascii_whitespace());
            let ident = decode_css_ident(trimmed).filter(|s| is_css_ident(s))?;
            // `::picker(...)` only accepts the literal `select` keyword as its argument.
            if name == "picker" && !ident.eq_ignore_ascii_case("select") {
                return None;
            }
            Some(format!("{name}({})", ident.to_lowercase()))
        }
        None => {
            if plain.contains(&name) {
                Some(name.to_string())
            } else {
                None // functional pseudo without an argument, or unknown name
            }
        }
    }
}

/// Whether `s` is a valid CSS identifier: non-empty, may not start with a digit (nor `-` followed
/// by a digit), and contains only name characters. Used to validate pseudo-element arguments.
pub(crate) fn is_css_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let valid_start = |c: char| c.is_ascii_alphabetic() || c == '_' || c == '-' || !c.is_ascii();
    if !valid_start(first) {
        return false;
    }
    // `-` alone, or `-` followed by a digit, is not a valid identifier start.
    if first == '-' {
        match s.chars().nth(1) {
            None => return false,
            Some(c) if c.is_ascii_digit() => return false,
            _ => {}
        }
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || !c.is_ascii())
}

/// Decode a CSS identifier that may contain escapes (`\61`, `\ `, …). Returns `None` if the input
/// isn't a valid identifier (contains a raw delimiter, etc.). Used to normalize pseudo-element
/// names and functional arguments coming from `getComputedStyle`'s string argument.
pub(crate) fn decode_css_ident(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            // CSS escape: either a hex sequence (1-6 hex digits, optional trailing whitespace) or
            // a literal escaped character.
            if i + 1 >= chars.len() {
                return None; // trailing backslash
            }
            let next = chars[i + 1];
            if next.is_ascii_hexdigit() {
                let mut hex = String::new();
                let mut k = i + 1;
                while k < chars.len() && hex.len() < 6 && chars[k].is_ascii_hexdigit() {
                    hex.push(chars[k]);
                    k += 1;
                }
                // One optional whitespace terminates the hex escape.
                if k < chars.len() && chars[k].is_ascii_whitespace() {
                    k += 1;
                }
                let cp = u32::from_str_radix(&hex, 16).ok()?;
                out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                i = k;
            } else {
                out.push(next);
                i += 2;
            }
        } else if c.is_ascii_alphanumeric() || c == '-' || c == '_' || !c.is_ascii() {
            out.push(c);
            i += 1;
        } else {
            return None; // raw delimiter (space, comma, paren, …) is not part of an identifier
        }
    }
    Some(out)
}

/// The result of normalizing `getComputedStyle`'s second (`pseudoElt`) argument per CSSOM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GcsPseudo {
    /// No pseudo (empty / null / a token that doesn't start with `:`) — use the element's own style.
    Element,
    /// A valid, recognized pseudo-element. Carries the canonical key (`"before"`, `"highlight(x)"`).
    Pseudo(String),
    /// A syntactically-valid-looking but unrecognized/invalid pseudo — yields an empty style.
    Invalid,
}

/// Normalize the `pseudoElt` argument of `getComputedStyle(elt, pseudoElt)` per the CSSOM
/// "legacy pseudo-element parsing" rules:
///   - empty / no leading `:` → ignore (use the element).
///   - one or two leading colons + a valid pseudo-element identifier (and nothing else) → that
///     pseudo-element; single-colon is legacy and only valid for before/after/first-line/first-letter.
///   - anything else (trailing tokens, unknown identifier, double-colon-required pseudos with a
///     single colon, malformed functional args) → invalid (empty style).
pub fn parse_gcs_pseudo(arg: &str) -> GcsPseudo {
    let chars: Vec<char> = arg.chars().collect();
    let n = chars.len();
    if n == 0 || chars[0] != ':' {
        return GcsPseudo::Element;
    }
    let double = n >= 2 && chars[1] == ':';
    let name_start = if double { 2 } else { 1 };
    // Read the identifier (name chars, including escapes — a backslash escapes the next run).
    let mut i = name_start;
    while i < n {
        let c = chars[i];
        if c == '\\' {
            // Consume the escape (hex run or single char) as part of the ident token.
            i += 1;
            if i < n && chars[i].is_ascii_hexdigit() {
                let mut len = 0;
                while i < n && len < 6 && chars[i].is_ascii_hexdigit() {
                    i += 1;
                    len += 1;
                }
                if i < n && chars[i].is_ascii_whitespace() {
                    i += 1;
                }
            } else if i < n {
                i += 1;
            }
        } else if c.is_ascii_alphanumeric() || c == '-' || c == '_' || !c.is_ascii() {
            i += 1;
        } else {
            break;
        }
    }
    let name_raw: String = chars[name_start..i].iter().collect();
    let Some(name) = decode_css_ident(&name_raw) else {
        return GcsPseudo::Invalid;
    };
    let name_l = name.to_ascii_lowercase();

    // Optional functional argument. Per the CSSOM legacy-pseudo grammar, an unterminated `(` is
    // tolerated (auto-closed at end of input): `::highlight(\nname` parses like `::highlight(name)`.
    let arg_opt: Option<String> = if i < n && chars[i] == '(' {
        let astart = i + 1;
        let mut k = astart;
        while k < n && chars[k] != ')' {
            k += 1;
        }
        let a: String = chars[astart..k].iter().collect();
        // Consume the `)` if present; otherwise we're at end of input (auto-closed).
        i = if k < n { k + 1 } else { k };
        Some(a)
    } else {
        None
    };

    // Nothing (except trailing whitespace? no — CSSOM forbids trailing tokens) may follow.
    if i != n {
        return GcsPseudo::Invalid;
    }

    // before/after (both colon forms) and the legacy four (single colon ok); everything else needs
    // double colon.
    let legacy_single = matches!(
        name_l.as_str(),
        "before" | "after" | "first-line" | "first-letter"
    );
    if !double && !legacy_single {
        return GcsPseudo::Invalid;
    }
    match name_l.as_str() {
        "before" if arg_opt.is_none() => GcsPseudo::Pseudo("before".to_string()),
        "after" if arg_opt.is_none() => GcsPseudo::Pseudo("after".to_string()),
        _ => match pseudo_element_key(&name_l, arg_opt.as_deref()) {
            Some(key) => GcsPseudo::Pseudo(key),
            None => GcsPseudo::Invalid,
        },
    }
}

/// Parse the inside of `[...]` into an [`AttrSel`].
/// Strip a CSS attribute-namespace prefix to the local name. `*|attr` (any namespace) and `|attr` /
/// bare `attr` (no namespace) all match our HTML attributes (which carry no namespace), so they
/// reduce to the local name. A specific `ns|attr` is left intact — we don't track per-attribute
/// namespaces, so it won't match a no-namespace attribute, which is the correct result for HTML.
pub(crate) fn strip_attr_namespace(name: &str) -> String {
    if let Some(rest) = name.strip_prefix("*|") {
        rest.to_string()
    } else if let Some(rest) = name.strip_prefix('|') {
        rest.to_string()
    } else {
        name.to_string()
    }
}

pub(crate) fn parse_attr(inner: &str) -> Option<AttrSel> {
    let s = inner.trim();
    // Detect a trailing ` i` / ` s` case flag (only meaningful with a value, but tolerate it).
    let mut case_insensitive = false;
    let mut body = s.to_string();
    {
        let lower = body.to_ascii_lowercase();
        if lower.ends_with(" i") {
            case_insensitive = true;
            let len = body.len() - 2;
            body.truncate(len);
            body = body.trim_end().to_string();
        } else if lower.ends_with(" s") {
            let len = body.len() - 2;
            body.truncate(len);
            body = body.trim_end().to_string();
        }
    }
    // Find the operator.
    let ops: [(&str, AttrOp); 6] = [
        ("~=", AttrOp::Includes),
        ("|=", AttrOp::DashMatch),
        ("^=", AttrOp::Prefix),
        ("$=", AttrOp::Suffix),
        ("*=", AttrOp::Substring),
        ("=", AttrOp::Equals),
    ];
    for (tok, op) in ops {
        if let Some(pos) = body.find(tok) {
            let name = body[..pos].trim().to_string();
            let raw_val = body[pos + tok.len()..].trim();
            let value = unquote(raw_val);
            if name.is_empty() {
                return None;
            }
            return Some(AttrSel {
                name: strip_attr_namespace(&name.to_ascii_lowercase()),
                op,
                value,
                case_insensitive,
            });
        }
    }
    // No operator → presence test.
    let name = body.trim().to_string();
    if name.is_empty() {
        return None;
    }
    Some(AttrSel {
        name: strip_attr_namespace(&name.to_ascii_lowercase()),
        op: AttrOp::Exists,
        value: String::new(),
        case_insensitive,
    })
}

/// Strip optional surrounding quotes.
pub(crate) fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 {
        let b = s.as_bytes();
        if (b[0] == b'"' && b[s.len() - 1] == b'"') || (b[0] == b'\'' && b[s.len() - 1] == b'\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// Parse a pseudo-class by name (+ optional functional argument). Returns the pseudo and its
/// specificity contribution. `None` only for genuinely unparseable functional args.
pub(crate) fn parse_pseudo(name: &str, arg: Option<&str>) -> Option<(Pseudo, Spec)> {
    let class_spec = Spec { a: 0, b: 1, c: 0 };
    let p = match name {
        "first-child" => Pseudo::FirstChild,
        "last-child" => Pseudo::LastChild,
        "only-child" => Pseudo::OnlyChild,
        "first-of-type" => Pseudo::FirstOfType,
        "last-of-type" => Pseudo::LastOfType,
        "only-of-type" => Pseudo::OnlyOfType,
        "root" => Pseudo::Root,
        "empty" => Pseudo::Empty,
        "checked" => Pseudo::Checked,
        "disabled" => Pseudo::Disabled,
        "enabled" => Pseudo::Enabled,
        "required" => Pseudo::Required,
        "optional" => Pseudo::Optional,
        "link" | "any-link" => Pseudo::Link,
        "visited" => Pseudo::Visited,
        "hover" => Pseudo::Hover,
        "focus" => Pseudo::Focus,
        "active" => Pseudo::Active,
        "focus-within" => Pseudo::FocusWithin,
        "focus-visible" => Pseudo::FocusVisible,
        // Best-effort never-match (parse cleanly, never match).
        "target" | "default" | "placeholder-shown" | "read-only" | "read-write" | "in-range"
        | "out-of-range" | "valid" | "invalid" | "indeterminate" | "autofill" => Pseudo::NeverMatch,
        "lang" => {
            let a = arg?.trim().trim_matches(|c| c == '"' || c == '\'').trim();
            if a.is_empty() {
                return None;
            }
            Pseudo::Lang(a.to_ascii_lowercase())
        }
        "nth-child" => Pseudo::NthChild(parse_nth(arg?)?),
        "nth-last-child" => Pseudo::NthLastChild(parse_nth(arg?)?),
        "nth-of-type" => Pseudo::NthOfType(parse_nth(arg?)?),
        "nth-last-of-type" => Pseudo::NthLastOfType(parse_nth(arg?)?),
        "not" => {
            let list = parse_selector_list(arg?)?;
            let s = list.iter().fold(Spec::default(), |acc, c| {
                acc.max_with(unpack_spec(c.specificity))
            });
            return Some((Pseudo::Not(list), s));
        }
        "is" | "matches" => {
            let list = parse_selector_list(arg?)?;
            let s = list.iter().fold(Spec::default(), |acc, c| {
                acc.max_with(unpack_spec(c.specificity))
            });
            return Some((Pseudo::Is(list), s));
        }
        "where" => {
            let list = parse_selector_list(arg?)?;
            // :where() contributes ZERO specificity.
            return Some((Pseudo::Where(list), Spec::default()));
        }
        // Unknown pseudo-class: best-effort never-match (don't drop the rule — the rest of the
        // compound may still be useful, but this element won't match it).
        _ => Pseudo::NeverMatch,
    };
    Some((p, class_spec))
}

/// Unpack a packed specificity back to components (for `:is`/`:not` "most specific arg").
pub(crate) fn unpack_spec(packed: u32) -> Spec {
    Spec {
        a: packed / 10000,
        b: (packed / 100) % 100,
        c: packed % 100,
    }
}

/// Parse a comma-separated selector list (the argument of `:is/:where/:not`).
pub(crate) fn parse_selector_list(arg: &str) -> Option<Vec<ComplexSelector>> {
    let mut out = Vec::new();
    for piece in split_selector_list(arg) {
        let p = piece.trim();
        if p.is_empty() {
            continue;
        }
        out.push(parse_complex(p)?);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Split a selector list on top-level commas (not inside [], (), or quotes).
pub(crate) fn split_selector_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut brk = 0i32;
    let mut par = 0i32;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            '[' => {
                brk += 1;
                cur.push(c);
            }
            ']' => {
                brk -= 1;
                cur.push(c);
            }
            '(' => {
                par += 1;
                cur.push(c);
            }
            ')' => {
                par -= 1;
                cur.push(c);
            }
            ',' if brk == 0 && par == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// Parse an `An+B` micro-syntax (`odd`, `even`, `3`, `2n`, `2n+1`, `-n+3`, `n`).
pub(crate) fn parse_nth(arg: &str) -> Option<NthArg> {
    let s: String = arg
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if s == "odd" {
        return Some(NthArg { a: 2, b: 1 });
    }
    if s == "even" {
        return Some(NthArg { a: 2, b: 0 });
    }
    if let Some(npos) = s.find('n') {
        let a_str = &s[..npos];
        let a = match a_str {
            "" | "+" => 1,
            "-" => -1,
            _ => a_str.parse::<i32>().ok()?,
        };
        let rest = &s[npos + 1..];
        let b = if rest.is_empty() {
            0
        } else {
            // rest is like "+1" / "-3".
            rest.parse::<i32>().ok()?
        };
        Some(NthArg { a, b })
    } else {
        // Plain integer B.
        Some(NthArg {
            a: 0,
            b: s.parse::<i32>().ok()?,
        })
    }
}

/// A valid CSS identifier for our purposes: letters, digits, `-`, `_`, not starting empty.
pub(crate) fn is_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_name_char)
}

/// A character allowed inside a class/id/type/pseudo name.
pub(crate) fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '\\' || !c.is_ascii()
}

// ===========================================================================================
// Matching against the tree (right-to-left, with backtracking)
// ===========================================================================================

/// Helper: borrow an element's [`ElementData`] for a node id, if it is an element.
pub(crate) fn el_of(doc: &dom::Document, id: dom::NodeId) -> Option<&dom::ElementData> {
    if id.0 >= doc.len() {
        return None;
    }
    match &doc.get(id).data {
        dom::NodeData::Element(e) => Some(e),
        _ => None,
    }
}

/// Element parent of `id` (skips non-element ancestors — though in practice the parent of an
/// element is the document or another element).
pub(crate) fn parent_of(doc: &dom::Document, id: dom::NodeId) -> Option<dom::NodeId> {
    doc.get(id).parent
}

/// Preceding *element* sibling of `id` (immediately before, skipping text/comment nodes).
pub(crate) fn prev_element_sibling(doc: &dom::Document, id: dom::NodeId) -> Option<dom::NodeId> {
    let parent = parent_of(doc, id)?;
    let kids = &doc.get(parent).children;
    let pos = kids.iter().position(|&c| c == id)?;
    kids[..pos]
        .iter()
        .rev()
        .copied()
        .find(|&c| el_of(doc, c).is_some())
}

/// All preceding element siblings of `id`, nearest-first.
pub(crate) fn prev_element_siblings(doc: &dom::Document, id: dom::NodeId) -> Vec<dom::NodeId> {
    let Some(parent) = parent_of(doc, id) else {
        return Vec::new();
    };
    let kids = &doc.get(parent).children;
    let Some(pos) = kids.iter().position(|&c| c == id) else {
        return Vec::new();
    };
    kids[..pos]
        .iter()
        .rev()
        .copied()
        .filter(|&c| el_of(doc, c).is_some())
        .collect()
}

/// Match a full complex selector against node `id` (right-to-left with backtracking).
pub(crate) fn complex_matches(doc: &dom::Document, id: dom::NodeId, sel: &ComplexSelector) -> bool {
    // Subject (parts[0]) must match `id`; then recurse leftward.
    if el_of(doc, id).is_none() {
        return false;
    }
    if !compound_matches(doc, id, &sel.parts[0].1) {
        return false;
    }
    match_from(doc, id, &sel.parts, 1)
}

/// Match the remaining parts `sel[idx..]` against the tree, given that `sel[idx-1]` matched at
/// `node`. Each part carries the combinator relating it to the part on its right.
pub(crate) fn match_from(
    doc: &dom::Document,
    node: dom::NodeId,
    parts: &[(Combinator, Compound)],
    idx: usize,
) -> bool {
    if idx >= parts.len() {
        return true;
    }
    let (comb, compound) = &parts[idx];
    match comb {
        Combinator::Subject => true, // shouldn't happen past index 0
        Combinator::Child => {
            if let Some(p) = parent_of(doc, node) {
                if el_of(doc, p).is_some() && compound_matches(doc, p, compound) {
                    return match_from(doc, p, parts, idx + 1);
                }
            }
            false
        }
        Combinator::Descendant => {
            // Try each ancestor; backtrack.
            let mut cur = parent_of(doc, node);
            while let Some(a) = cur {
                if el_of(doc, a).is_some()
                    && compound_matches(doc, a, compound)
                    && match_from(doc, a, parts, idx + 1)
                {
                    return true;
                }
                cur = parent_of(doc, a);
            }
            false
        }
        Combinator::NextSibling => {
            if let Some(s) = prev_element_sibling(doc, node) {
                if compound_matches(doc, s, compound) {
                    return match_from(doc, s, parts, idx + 1);
                }
            }
            false
        }
        Combinator::SubsequentSibling => {
            for s in prev_element_siblings(doc, node) {
                if compound_matches(doc, s, compound) && match_from(doc, s, parts, idx + 1) {
                    return true;
                }
            }
            false
        }
    }
}

/// Does node `id` (which must be an element) match a single compound selector?
pub(crate) fn compound_matches(doc: &dom::Document, id: dom::NodeId, c: &Compound) -> bool {
    let Some(el) = el_of(doc, id) else {
        return false;
    };
    if let Some(t) = &c.type_part {
        if !el.tag.eq_ignore_ascii_case(t) {
            return false;
        }
    }
    // Namespace constraint on the (possibly implied universal) type selector. Only enforced when an
    // `@namespace` rule is in scope; otherwise every constraint matches (so non-namespaced pages are
    // unaffected). `el.namespace == None` means the HTML namespace.
    if !namespace_matches(&c.type_ns, el.namespace.as_deref()) {
        return false;
    }
    for want in &c.ids {
        match el.id() {
            Some(eid) if eid == want => {}
            _ => return false,
        }
    }
    for class in &c.classes {
        if !el.classes().any(|cl| cl == class) {
            return false;
        }
    }
    for attr in &c.attrs {
        if !attr_matches(el, attr) {
            return false;
        }
    }
    for p in &c.pseudos {
        if !pseudo_matches(doc, id, el, p) {
            return false;
        }
    }
    true
}

/// The XHTML namespace URI. An element with `namespace == None` (a normal HTML element) is treated
/// as being in this namespace for `@namespace` matching.
pub(crate) const XHTML_NS: &str = "http://www.w3.org/1999/xhtml";

/// Does a selector's namespace constraint match an element's namespace? `el_ns` is the element's
/// namespace URI (`None` = HTML, treated as the XHTML namespace). Only enforced when an
/// `@namespace` environment is in scope; with no `@namespace` rules every constraint passes, so
/// ordinary (non-namespaced) pages match exactly as they did before this feature.
pub(crate) fn namespace_matches(constraint: &NsConstraint, el_ns: Option<&str>) -> bool {
    // The common case: no namespace component on the selector and no `@namespace` rule in scope.
    // Resolve without touching the bindings (and never constrain) so non-namespaced pages are fast
    // and unaffected.
    if *constraint == NsConstraint::Unspecified {
        return NAMESPACE_BINDINGS.with(|c| {
            let env = c.borrow();
            match &env.default_ns {
                // With a declared default namespace, an unspecified type is constrained to it.
                Some(def) => el_ns.unwrap_or(XHTML_NS) == def,
                None => true, // no default → matches any namespace
            }
        });
    }
    NAMESPACE_BINDINGS.with(|c| {
        let env = c.borrow();
        // The element's effective namespace URI (HTML elements default to XHTML).
        let el_uri = el_ns.unwrap_or(XHTML_NS);
        match constraint {
            NsConstraint::Any => true,
            // `|tag` = the null namespace: matches only elements explicitly in no namespace.
            NsConstraint::None => el_uri.is_empty(),
            NsConstraint::Unspecified => match &env.default_ns {
                // With a default namespace, an unspecified type is constrained to it.
                Some(def) => el_uri == def,
                // No default namespace → matches any namespace.
                None => true,
            },
            NsConstraint::Prefixed(prefix) => match env.lookup(prefix) {
                Some(uri) => el_uri == uri,
                // Unknown prefix → matches nothing (invalid selector, treated as non-matching).
                None => false,
            },
        }
    })
}

/// Match one attribute selector against an element. Attribute *names* are matched
/// case-insensitively (HTML); values per the operator and the `i` flag.
pub(crate) fn attr_matches(el: &dom::ElementData, a: &AttrSel) -> bool {
    // Find the attribute case-insensitively by name.
    let actual = el
        .attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(&a.name))
        .map(|(_, v)| v.as_str());
    let Some(val) = actual else {
        return false;
    };
    if a.op == AttrOp::Exists {
        return true;
    }
    let (hay, needle) = if a.case_insensitive {
        (val.to_ascii_lowercase(), a.value.to_ascii_lowercase())
    } else {
        (val.to_string(), a.value.clone())
    };
    match a.op {
        AttrOp::Exists => true,
        AttrOp::Equals => hay == needle,
        AttrOp::Includes => !needle.is_empty() && hay.split_whitespace().any(|w| w == needle),
        AttrOp::DashMatch => hay == needle || hay.starts_with(&format!("{needle}-")),
        AttrOp::Prefix => !needle.is_empty() && hay.starts_with(&needle),
        AttrOp::Suffix => !needle.is_empty() && hay.ends_with(&needle),
        AttrOp::Substring => !needle.is_empty() && hay.contains(&needle),
    }
}

/// Match a pseudo-class against an element node.
pub(crate) fn pseudo_matches(
    doc: &dom::Document,
    id: dom::NodeId,
    el: &dom::ElementData,
    p: &Pseudo,
) -> bool {
    match p {
        Pseudo::Root => el.tag.eq_ignore_ascii_case("html"),
        Pseudo::FirstChild => element_index(doc, id).map(|(i, _)| i == 0).unwrap_or(false),
        Pseudo::LastChild => element_index(doc, id)
            .map(|(i, t)| i + 1 == t)
            .unwrap_or(false),
        Pseudo::OnlyChild => element_index(doc, id).map(|(_, t)| t == 1).unwrap_or(false),
        Pseudo::NthChild(n) => element_index(doc, id)
            .map(|(i, _)| n.matches(i as i32 + 1))
            .unwrap_or(false),
        Pseudo::NthLastChild(n) => element_index(doc, id)
            .map(|(i, t)| n.matches((t - i) as i32))
            .unwrap_or(false),
        Pseudo::FirstOfType => type_index(doc, id, &el.tag)
            .map(|(i, _)| i == 0)
            .unwrap_or(false),
        Pseudo::LastOfType => type_index(doc, id, &el.tag)
            .map(|(i, t)| i + 1 == t)
            .unwrap_or(false),
        Pseudo::OnlyOfType => type_index(doc, id, &el.tag)
            .map(|(_, t)| t == 1)
            .unwrap_or(false),
        Pseudo::NthOfType(n) => type_index(doc, id, &el.tag)
            .map(|(i, _)| n.matches(i as i32 + 1))
            .unwrap_or(false),
        Pseudo::NthLastOfType(n) => type_index(doc, id, &el.tag)
            .map(|(i, t)| n.matches((t - i) as i32))
            .unwrap_or(false),
        Pseudo::Empty => is_empty_element(doc, id),
        Pseudo::Checked => {
            (el.tag.eq_ignore_ascii_case("input") || el.tag.eq_ignore_ascii_case("option"))
                && el.attrs.keys().any(|k| {
                    k.eq_ignore_ascii_case("checked") || k.eq_ignore_ascii_case("selected")
                })
        }
        Pseudo::Disabled => is_form_control(&el.tag) && has_attr(el, "disabled"),
        Pseudo::Enabled => is_form_control(&el.tag) && !has_attr(el, "disabled"),
        Pseudo::Required => is_form_control(&el.tag) && has_attr(el, "required"),
        Pseudo::Optional => is_form_control(&el.tag) && !has_attr(el, "required"),
        Pseudo::Link => el.tag.eq_ignore_ascii_case("a") && has_attr(el, "href"),
        // A link to the current page (empty / pure-fragment href) is in history → visited.
        Pseudo::Visited => {
            el.tag.eq_ignore_ascii_case("a")
                && el.attrs.get("href").is_some_and(|h| {
                    let h = h.trim();
                    h.is_empty() || h.starts_with('#')
                })
        }
        Pseudo::Hover => {
            let h = interaction_hovered();
            h == Some(id.0)
                || h.map(|hn| is_ancestor(doc, id, dom::NodeId(hn)))
                    .unwrap_or(false)
        }
        // `:active` ≈ `:hover` (no separate pressed-state tracking in the engine).
        Pseudo::Active => {
            let h = interaction_hovered();
            h == Some(id.0)
                || h.map(|hn| is_ancestor(doc, id, dom::NodeId(hn)))
                    .unwrap_or(false)
        }
        Pseudo::Focus | Pseudo::FocusVisible => interaction_focused() == Some(id.0),
        Pseudo::FocusWithin => {
            let f = interaction_focused();
            f == Some(id.0)
                || f.map(|fn_| is_ancestor(doc, id, dom::NodeId(fn_)))
                    .unwrap_or(false)
        }
        Pseudo::Lang(want) => element_lang(doc, id)
            .map(|l| {
                let l = l.to_ascii_lowercase();
                l == *want || l.starts_with(&format!("{want}-"))
            })
            .unwrap_or(false),
        Pseudo::Not(list) => !list.iter().any(|s| complex_matches(doc, id, s)),
        Pseudo::Is(list) | Pseudo::Where(list) => list.iter().any(|s| complex_matches(doc, id, s)),
        Pseudo::NeverMatch => false,
    }
}

pub(crate) fn has_attr(el: &dom::ElementData, name: &str) -> bool {
    el.attrs.keys().any(|k| k.eq_ignore_ascii_case(name))
}

/// The content language of element `id` per HTML: the `lang` (or `xml:lang`) attribute of the
/// nearest inclusive ancestor that has one. Returns `None` if no ancestor sets a language.
pub(crate) fn element_lang(doc: &dom::Document, id: dom::NodeId) -> Option<String> {
    let mut cur = Some(id);
    while let Some(n) = cur {
        if let Some(el) = el_of(doc, n) {
            if let Some((_, v)) = el
                .attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("lang") || k.eq_ignore_ascii_case("xml:lang"))
            {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
        cur = parent_of(doc, n);
    }
    None
}

pub(crate) fn is_form_control(tag: &str) -> bool {
    matches!(
        tag.to_ascii_lowercase().as_str(),
        "input" | "button" | "select" | "textarea" | "option" | "optgroup" | "fieldset"
    )
}

/// Is `ancestor` an ancestor of `descendant` (strictly above it)?
pub(crate) fn is_ancestor(
    doc: &dom::Document,
    ancestor: dom::NodeId,
    descendant: dom::NodeId,
) -> bool {
    if descendant.0 >= doc.len() {
        return false;
    }
    let mut cur = doc.get(descendant).parent;
    while let Some(p) = cur {
        if p == ancestor {
            return true;
        }
        cur = doc.get(p).parent;
    }
    false
}

/// (index-among-element-siblings, total-element-siblings) for `id`.
pub(crate) fn element_index(doc: &dom::Document, id: dom::NodeId) -> Option<(usize, usize)> {
    let parent = parent_of(doc, id)?;
    let kids = &doc.get(parent).children;
    let elems: Vec<dom::NodeId> = kids
        .iter()
        .copied()
        .filter(|&c| el_of(doc, c).is_some())
        .collect();
    let pos = elems.iter().position(|&c| c == id)?;
    Some((pos, elems.len()))
}

/// (index-among-same-type-siblings, total-same-type-siblings) for `id`.
pub(crate) fn type_index(
    doc: &dom::Document,
    id: dom::NodeId,
    tag: &str,
) -> Option<(usize, usize)> {
    let parent = parent_of(doc, id)?;
    let kids = &doc.get(parent).children;
    let same: Vec<dom::NodeId> = kids
        .iter()
        .copied()
        .filter(|&c| {
            el_of(doc, c)
                .map(|e| e.tag.eq_ignore_ascii_case(tag))
                .unwrap_or(false)
        })
        .collect();
    let pos = same.iter().position(|&c| c == id)?;
    Some((pos, same.len()))
}

/// `:empty` — no element children and no non-whitespace text.
pub(crate) fn is_empty_element(doc: &dom::Document, id: dom::NodeId) -> bool {
    for &c in &doc.get(id).children {
        if c.0 >= doc.len() {
            continue;
        }
        match &doc.get(c).data {
            dom::NodeData::Element(_) => return false,
            dom::NodeData::Text(t) if !t.trim().is_empty() => return false,
            _ => {}
        }
    }
    true
}

/// The built-in user-agent stylesheet: sane defaults on a white page canvas.
pub(crate) fn user_agent_stylesheet() -> css::Stylesheet {
    // html/body default text color is themed by the root's used `color-scheme` (resolved by the
    // cascade pre-pass before this runs): black on a light page, light grey (`#e8e8e8`) on a dark
    // one. Form-control text/background (`input`/`button`/…) is intentionally left light — control
    // & scrollbar theming is out of scope.
    let (tr, tg, tb) = ua_default_text_color();
    let text = format!("#{tr:02x}{tg:02x}{tb:02x}");
    let sheet =
        // html/body keep explicit UA color rules (rather than dropping them) so body's color doesn't
        // inherit a `:root` author color the way a real `:root` selector would; author rules still
        // override.
        "html { color: {TEXT}; font-size: 16px }
         body { color: {TEXT}; font-size: 16px }
         h1 { font-size: 32px; font-weight: bold; display: block; margin: 0.67em 0 }
         h2 { font-size: 26px; font-weight: bold; display: block; margin: 0.83em 0 }
         h3 { font-size: 20px; font-weight: bold; display: block; margin: 1em 0 }
         h4 { font-size: 17px; font-weight: bold; display: block; margin: 1.33em 0 }
         h5 { font-size: 15px; font-weight: bold; display: block; margin: 1.67em 0 }
         h6 { font-size: 13px; font-weight: bold; display: block; margin: 2.33em 0 }
         p { display: block; margin: 1em 0 }
         svg { forced-color-adjust: none }
         div { display: block }
         section { display: block }
         article { display: block }
         header { display: block }
         footer { display: block }
         nav { display: block }
         main { display: block }
         aside { display: block }
         ul { display: block; margin: 1em 0; padding-left: 40px; list-style-type: disc }
         ol { display: block; margin: 1em 0; padding-left: 40px; list-style-type: decimal }
         li { display: block }
         blockquote { display: block; margin: 1em 40px }
         pre { display: block; margin: 1em 0; white-space: pre }
         table { display: table }
         tr { display: table-row }
         td, th { display: table-cell; padding: 1px }
         th { font-weight: bold; text-align: center }
         thead { display: table-header-group }
         tbody { display: table-row-group }
         tfoot { display: table-footer-group }
         colgroup { display: table-column-group }
         col { display: table-column }
         details { display: block }
         summary { display: block }
         figure { display: block; margin: 1em 40px }
         figcaption { display: block }
         fieldset { display: block }
         legend { display: block }
         form { display: block }
         dl { display: block; margin: 1em 0 }
         dt { display: block }
         dd { display: block; margin-left: 40px }
         address { display: block }
         dialog { display: none; margin: auto; padding: 1em; border: 2px solid #767676; background-color: #ffffff; color: #000000 }
         dialog[open] { display: block }
         hr { display: block; margin: 0.5em 0; height: 1px; background-color: #888888; border-top: 1px solid #888888 }
         caption { display: table-caption }
         details:not([open]) > :not(summary) { display: none }
         summary::before { content: \"\\25B8 \" }
         details[open] > summary::before { content: \"\\25BE \" }
         b { font-weight: bold }
         strong { font-weight: bold }
         i { font-style: italic }
         em { font-style: italic }
         a { text-decoration: underline; color: #0000ee }
         u, ins { text-decoration: underline }
         s, del, strike { text-decoration: line-through }
         abbr[title] { text-decoration: underline }
         mark { background-color: Mark; color: MarkText }
         cite, var, dfn, address { font-style: italic }
         small { font-size: smaller }
         sub, sup { font-size: smaller }
         sub { vertical-align: sub }
         sup { vertical-align: super }
         q::before { content: \"\\201C\" }
         q::after { content: \"\\201D\" }
         input, textarea, select, button { display: inline-block; border: 1px solid #767676; color: #000000; background-color: #ffffff; padding: 1px 2px }
         input[type=submit], input[type=reset], input[type=button], button { background-color: #efefef; padding: 2px 8px }
         input[type=file] { background-color: #efefef; padding: 1px 2px }
         input[type=checkbox], input[type=radio], input[type=range], input[type=color], progress, meter { border: 0; padding: 0; background-color: transparent }
         label { display: inline-block }";
    css::parse(&sheet.replace("{TEXT}", &text))
}
