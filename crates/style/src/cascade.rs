use crate::*;
use std::collections::HashMap;
use std::sync::Arc;

/// Compute a [`ComputedStyle`] for every element node in `doc`, using the built-in UA
/// stylesheet first, then the supplied author `sheets` (in document order), then each
/// element's inline `style="…"` attribute (highest precedence within an element).
pub fn cascade(
    doc: &dom::Document,
    sheets: &[css::Stylesheet],
) -> HashMap<dom::NodeId, ComputedStyle> {
    cascade_locked(doc, sheets, None).0
}

/// Like [`cascade`], but `os_dark` is the OS appearance for this document (true = Dark), applied
/// to `@media (prefers-color-scheme)` and the `color-scheme` resolution *atomically* with the
/// cascade (set under the cascade lock so a concurrent cascade can't clobber the shared flag), and
/// also returns the root's resolved *used* color scheme (true = dark). The engine stores that on
/// its layout cache so the canvas background doesn't have to re-read the racy process-global. Pass
/// this rather than calling [`set_color_scheme_dark`] separately before [`cascade`].
pub fn cascade_with_root_scheme(
    doc: &dom::Document,
    sheets: &[css::Stylesheet],
    os_dark: bool,
) -> (HashMap<dom::NodeId, ComputedStyle>, bool) {
    cascade_locked(doc, sheets, Some(os_dark))
}

/// The shared, lock-held cascade body. When `os_dark` is `Some`, the OS-appearance flag is set
/// under the lock first (so `@media (prefers-color-scheme)` and the `color-scheme` resolution see a
/// stable value); when `None`, the previously-set global is used as-is (back-compat for callers
/// that set it themselves). Returns the styles plus the root's resolved used color scheme.
pub(crate) fn cascade_locked(
    doc: &dom::Document,
    sheets: &[css::Stylesheet],
    os_dark: Option<bool>,
) -> (HashMap<dom::NodeId, ComputedStyle>, bool) {
    // Hold the cascade lock for the whole body: the OS-appearance flag and the root-color-scheme
    // global are written and read back here, so concurrent cascades must not interleave.
    let _cascade_guard = CASCADE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(dark) = os_dark {
        set_color_scheme_dark(dark);
    }
    // Collect `@property` registrations from all author sheets (later sheets / later rules win on
    // duplicate names, matching the cascade-order "last registration wins" behaviour).
    {
        let mut reg: Vec<css::PropertyRule> = Vec::new();
        for sheet in sheets {
            for pr in &sheet.property_rules {
                reg.retain(|r| r.name != pr.name);
                reg.push(pr.clone());
            }
        }
        REGISTERED_PROPERTIES.with(|c| *c.borrow_mut() = reg);
    }
    // Collect `@namespace` bindings from all author sheets (later declarations win on duplicate
    // prefix / default).
    {
        let mut env = NamespaceEnv::default();
        for sheet in sheets {
            for ns in &sheet.namespace_rules {
                if ns.prefix.is_empty() {
                    env.default_ns = Some(ns.uri.clone());
                } else {
                    env.prefixes.retain(|(p, _)| p != &ns.prefix);
                    env.prefixes.push((ns.prefix.clone(), ns.uri.clone()));
                }
            }
        }
        NAMESPACE_BINDINGS.with(|c| *c.borrow_mut() = env);
    }
    let mut out = HashMap::new();
    // Pre-pass: resolve the root's *used* color scheme (light vs dark) BEFORE the real UA sheet and
    // cascade are built, so the UA dark defaults (html/body text color in `user_agent_stylesheet`,
    // initial color via `ComputedStyle::default()`, canvas background via `ua_default_canvas_color`)
    // are seeded for this whole cascade. `color-scheme` is often gated behind
    // `@media (prefers-color-scheme: dark)`, so we cascade `<html>` (then `<body>`) for real to read
    // the property, then combine with the OS flag. `<meta name="color-scheme">` is a fallback opt-in
    // mapped like the property. See `resolve_root_color_scheme` (it runs with light defaults so its
    // own read can't depend on the result).
    set_root_used_scheme_dark(resolve_root_color_scheme(doc, sheets));
    // Now build the (themed) UA sheet and the selector index ONCE over UA + author sheets, so every
    // node shares it instead of re-scanning (and re-parsing) all rules per element.
    let ua = user_agent_stylesheet();
    let index = SelectorIndex::build(&ua, sheets);
    // The root inherits from a fresh default style (now themed by the resolved scheme above).
    let initial = ComputedStyle::default();
    // Custom properties (`--name`) inherit; the root starts with an empty environment.
    let initial_vars: Arc<HashMap<String, String>> = Arc::new(HashMap::new());
    cascade_node(
        doc,
        doc.root(),
        &initial,
        &initial_vars,
        false,
        &index,
        &mut out,
    );
    (out, root_used_scheme_dark())
}

/// Cascade only the subtree rooted at `root_id` (e.g. an `<iframe>` facade document's body),
/// returning computed styles for it and its descendants. Author `sheets` apply; `@media` queries
/// evaluate against the CURRENT viewport (callers set [`set_viewport_metrics`] to the iframe's own
/// size first, so the iframe gets its own media context). The subtree root inherits initial values.
pub fn cascade_subtree(
    doc: &dom::Document,
    root_id: dom::NodeId,
    sheets: &[css::Stylesheet],
) -> HashMap<dom::NodeId, ComputedStyle> {
    let _cascade_guard = CASCADE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // @property registrations from this document's sheets (last wins on duplicate names).
    {
        let mut reg: Vec<css::PropertyRule> = Vec::new();
        for sheet in sheets {
            for pr in &sheet.property_rules {
                reg.retain(|r| r.name != pr.name);
                reg.push(pr.clone());
            }
        }
        REGISTERED_PROPERTIES.with(|c| *c.borrow_mut() = reg);
    }
    // @namespace bindings from this document's sheets.
    {
        let mut env = NamespaceEnv::default();
        for sheet in sheets {
            for ns in &sheet.namespace_rules {
                if ns.prefix.is_empty() {
                    env.default_ns = Some(ns.uri.clone());
                } else {
                    env.prefixes.push((ns.prefix.clone(), ns.uri.clone()));
                }
            }
        }
        NAMESPACE_BINDINGS.with(|c| *c.borrow_mut() = env);
    }
    let ua = user_agent_stylesheet();
    let index = SelectorIndex::build(&ua, sheets);
    let initial = ComputedStyle::default();
    let initial_vars: Arc<HashMap<String, String>> = Arc::new(HashMap::new());
    let mut out = HashMap::new();
    cascade_node(
        doc,
        root_id,
        &initial,
        &initial_vars,
        false,
        &index,
        &mut out,
    );
    out
}

/// The current viewport metrics `(width, height, device_pixel_ratio)` in CSS px — what `@media`
/// queries are evaluated against. Lets callers save/restore around a temporary override (e.g. an
/// iframe's own size for a subtree cascade).
pub fn viewport_metrics() -> (f32, f32, f32) {
    use std::sync::atomic::Ordering;
    (
        f32::from_bits(VIEWPORT_W_BITS.load(Ordering::Relaxed)),
        f32::from_bits(VIEWPORT_H_BITS.load(Ordering::Relaxed)),
        f32::from_bits(VIEWPORT_DPR_BITS.load(Ordering::Relaxed)),
    )
}

/// Resolve the root's *used* color scheme (true = dark) for one cascade. Reads the page's
/// `color-scheme` opt-in (which determines whether the UA renders a dark canvas + light text) and
/// combines it with the OS appearance:
///
/// 1. Cascade `<html>` for real (so a `color-scheme` set under `@media (prefers-color-scheme:dark)`
///    or via `:root{…}` is picked up), then fall back to `<body>` if `<html>` left it `Normal`.
/// 2. If still `Normal`, honor a `<meta name="color-scheme" content="…">` in `<head>`, mapped like
///    the property.
/// 3. Apply [`ColorScheme::resolves_dark`] against the OS flag: only-dark → dark; only-light/normal
///    → light; `light dark` (both) → follow the OS.
///
/// Runs the pre-pass with the dark UA defaults *disabled* (`set_root_used_scheme_dark(false)`) so
/// the property read doesn't depend on its own result. The caller stores the returned value.
pub(crate) fn resolve_root_color_scheme(doc: &dom::Document, sheets: &[css::Stylesheet]) -> bool {
    // Read the property with light defaults so the pre-pass result can't depend on itself.
    set_root_used_scheme_dark(false);
    // Build a (light-themed) UA sheet + index just for this read. `color-scheme` only ever comes
    // from author CSS / inline style / meta, so the UA rules don't affect the result, but we still
    // index over UA + author to match real selectors (e.g. `:root { color-scheme: dark }`).
    let ua = user_agent_stylesheet();
    let index = SelectorIndex::build(&ua, sheets);
    let initial = ComputedStyle::default();
    let initial_vars: Arc<HashMap<String, String>> = Arc::new(HashMap::new());

    let mut scheme = ColorScheme::Normal;
    if let Some(html) = find_element(doc, "html") {
        if let dom::NodeData::Element(el) = &doc.get(html).data {
            let (s, _) =
                compute_element_style(doc, html, el, &initial, &initial_vars, false, &index);
            scheme = s.color_scheme;
        }
    }
    if scheme == ColorScheme::Normal {
        if let Some(body) = find_element(doc, "body") {
            if let dom::NodeData::Element(el) = &doc.get(body).data {
                let (s, _) =
                    compute_element_style(doc, body, el, &initial, &initial_vars, false, &index);
                scheme = s.color_scheme;
            }
        }
    }
    if scheme == ColorScheme::Normal {
        if let Some(meta) = meta_color_scheme(doc) {
            scheme = meta;
        }
    }
    scheme.resolves_dark(color_scheme_dark())
}

/// Depth-first search for the first element with the given (lowercase) tag name.
pub(crate) fn find_element(doc: &dom::Document, tag: &str) -> Option<dom::NodeId> {
    fn walk(doc: &dom::Document, id: dom::NodeId, tag: &str) -> Option<dom::NodeId> {
        if id.0 >= doc.len() {
            return None;
        }
        if let dom::NodeData::Element(el) = &doc.get(id).data {
            if el.tag.eq_ignore_ascii_case(tag) {
                return Some(id);
            }
        }
        for &c in &doc.get(id).children {
            if let Some(found) = walk(doc, c, tag) {
                return Some(found);
            }
        }
        None
    }
    walk(doc, doc.root(), tag)
}

/// Read a `<meta name="color-scheme" content="…">` (the HTML opt-in equivalent of the CSS
/// property) and map its `content` like `color-scheme`. Returns the first such meta's value.
pub(crate) fn meta_color_scheme(doc: &dom::Document) -> Option<ColorScheme> {
    fn walk(doc: &dom::Document, id: dom::NodeId) -> Option<ColorScheme> {
        if id.0 >= doc.len() {
            return None;
        }
        if let dom::NodeData::Element(el) = &doc.get(id).data {
            if el.tag.eq_ignore_ascii_case("meta")
                && el
                    .attrs
                    .get("name")
                    .is_some_and(|n| n.eq_ignore_ascii_case("color-scheme"))
            {
                if let Some(content) = el.attrs.get("content") {
                    if let Some(cs) = parse_color_scheme(&content.to_ascii_lowercase()) {
                        return Some(cs);
                    }
                }
            }
        }
        for &c in &doc.get(id).children {
            if let Some(found) = walk(doc, c) {
                return Some(found);
            }
        }
        None
    }
    walk(doc, doc.root())
}

/// One indexed selector. Points back at the rule's declarations and carries everything needed
/// to confirm a full compound match and slot the result into the cascade ordering.
pub(crate) struct Entry<'a> {
    /// 0 = UA origin, 1 = author origin (matches `MatchEntry.origin`).
    pub(crate) origin: u8,
    /// Global source order, incremented across UA rules then author rules in sheet/rule order
    /// — identical to the `order` the brute-force scan assigns.
    pub(crate) order: usize,
    /// The compiled selector this entry was indexed under (used to verify the full compound).
    pub(crate) compiled: Compiled,
    /// The rule's declarations (applied as a unit when any of its selectors match).
    pub(crate) decls: &'a [(String, String)],
    /// The owning stylesheet's base URL (for resolving relative `url(...)` in `mask-image` etc.
    /// against the stylesheet, not the document). `None` if the sheet was parsed without a base.
    pub(crate) base: Option<&'a str>,
}

/// An index over all UA + author selectors, bucketed most-selective-key-first so a given
/// element only has to test the handful of rules that could plausibly match it (those keyed
/// by its id, one of its classes, its tag, or the universal/`:root` catch-all) instead of
/// every rule in every sheet.
///
/// Built once per [`cascade`]. Rules whose `@media`/`@container` doesn't apply are dropped at
/// build time (those conditions don't depend on the element). Selectors that the matcher would
/// never match (combinators etc.) are dropped entirely.
pub(crate) struct SelectorIndex<'a> {
    pub(crate) by_id: HashMap<String, Vec<Entry<'a>>>,
    pub(crate) by_class: HashMap<String, Vec<Entry<'a>>>,
    pub(crate) by_type: HashMap<String, Vec<Entry<'a>>>,
    pub(crate) universal: Vec<Entry<'a>>,
}

impl<'a> SelectorIndex<'a> {
    pub(crate) fn build(
        ua: &'a css::Stylesheet,
        author: &'a [css::Stylesheet],
    ) -> SelectorIndex<'a> {
        let mut idx = SelectorIndex {
            by_id: HashMap::new(),
            by_class: HashMap::new(),
            by_type: HashMap::new(),
            universal: Vec::new(),
        };
        let mut order = 0usize;
        // UA rules first, then author rules — preserving the exact global ordering the
        // brute-force scan assigns (order increments across every rule whether or not it is
        // indexed).
        for rule in &ua.rules {
            idx.add_rule(rule, 0, order);
            order += 1;
        }
        for sheet in author {
            for rule in &sheet.rules {
                idx.add_rule(rule, 1, order);
                order += 1;
            }
        }
        idx
    }

    /// Index every (indexable) selector of one rule, unless its media/container precludes it.
    fn add_rule(&mut self, rule: &'a css::Rule, origin: u8, order: usize) {
        // media/container don't depend on the element, so evaluate once here and skip the
        // whole rule if it doesn't apply (it can never contribute to any element).
        if !(media_applies(rule.media.as_deref()) && container_applies(rule.container.as_deref())) {
            return;
        }
        for sel in &rule.selectors {
            let Some(compiled) = compile_selector(sel) else {
                continue; // unsupported selector (e.g. pseudo-element) — drop it
            };
            // Bucket under the rightmost (subject) compound's most-selective simple part.
            match compiled.bucket_key().clone() {
                BucketKey::Id(id) => self.by_id.entry(id).or_default(),
                BucketKey::Class(class) => self.by_class.entry(class).or_default(),
                BucketKey::Type(t) => self.by_type.entry(t).or_default(),
                BucketKey::Universal => &mut self.universal,
            }
            .push(Entry {
                origin,
                order,
                compiled,
                decls: &rule.declarations,
                base: rule.base_url.as_deref(),
            });
        }
    }
}

// Live viewport metrics used to evaluate media queries (`min-width`/`max-width`/resolution),
// `@container` conditions, and viewport units (`vw`/`vh`/`%`) during the cascade. The engine sets
// these via `set_viewport_metrics` before each cascade, so they reflect the real window size and
// backing scale — and because the cascade re-runs on resize, media/container queries and viewport
// units respond to window resizing. Stored as f32 bits in atomics (0 = unset → fall back below).
pub(crate) static VIEWPORT_W_BITS: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);
pub(crate) static VIEWPORT_H_BITS: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);
pub(crate) static VIEWPORT_DPR_BITS: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// Live OS appearance used to evaluate `@media (prefers-color-scheme: dark|light)` during the
/// cascade. `true` = Dark. The engine sets this via [`set_color_scheme_dark`] on launch and on
/// every Light/Dark toggle; the cascade re-runs (layout cache invalidated) so dark-mode stylesheet
/// rules take effect. Mirrors the same flag in the `js` crate (which drives the JS `matchMedia`).
pub(crate) static COLOR_SCHEME_DARK: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set the logical viewport size (CSS px) and device pixel ratio used by the cascade for media
/// queries and viewport units. Call before [`cascade`] whenever the viewport changes.
pub fn set_viewport_metrics(width: f32, height: f32, device_pixel_ratio: f32) {
    use std::sync::atomic::Ordering;
    VIEWPORT_W_BITS.store(width.max(1.0).to_bits(), Ordering::Relaxed);
    VIEWPORT_H_BITS.store(height.max(1.0).to_bits(), Ordering::Relaxed);
    VIEWPORT_DPR_BITS.store(device_pixel_ratio.max(0.1).to_bits(), Ordering::Relaxed);
}

/// Set whether the effective OS appearance is Dark, used to evaluate
/// `@media (prefers-color-scheme: dark|light)` in the cascade. Call before [`cascade`] (the engine
/// does this on launch and on every appearance toggle).
pub fn set_color_scheme_dark(is_dark: bool) {
    COLOR_SCHEME_DARK.store(is_dark, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the effective OS appearance is currently Dark (drives `prefers-color-scheme`).
pub(crate) fn color_scheme_dark() -> bool {
    COLOR_SCHEME_DARK.load(std::sync::atomic::Ordering::Relaxed)
}

/// The root's *used* color scheme (true = dark), resolved by [`cascade`] from the page's
/// `color-scheme` (off `<html>`/`<body>`/`<meta>`) combined with the OS appearance. Seeds the dark
/// UA defaults: the initial/inherited text color ([`ua_default_text_color`]) and the canvas
/// background ([`ua_default_canvas_color`], read by the engine's `page_background`). Defaults to
/// light (false). Re-resolved every cascade, so an OS Light/Dark toggle (which re-runs the cascade)
/// flips both the `@media` gating the page's `color-scheme` AND this used scheme.
pub(crate) static ROOT_USED_SCHEME_DARK: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Serializes [`cascade`] across threads. `cascade` resolves the root color scheme into the
/// process-global [`ROOT_USED_SCHEME_DARK`] and then reads it back while building UA defaults, so
/// two concurrent cascades on different documents could otherwise clobber each other's flag. The
/// engine runs one cascade at a time, so this only matters for parallel `cargo test`; the lock is
/// cheap and held only for the (fast) cascade body. Poisoning is irrelevant — we only need mutual
/// exclusion — so the guard ignores it.
pub(crate) static CASCADE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

thread_local! {
    /// Registered custom properties (`@property`) in effect for the current cascade. Set at the
    /// start of [`cascade_locked`] from the author sheets; read by `compute_element_style` to seed
    /// each element's registered-property initial values into its `custom_props` environment. Only
    /// populated for the duration of one cascade (which holds [`CASCADE_LOCK`], so this is
    /// effectively single-threaded). Keyed by property name (`--x`).
    pub(crate) static REGISTERED_PROPERTIES: std::cell::RefCell<Vec<css::PropertyRule>> =
        const { std::cell::RefCell::new(Vec::new()) };

    /// `@namespace` prefix bindings in effect for the current cascade: `(prefix -> uri)` plus an
    /// optional default namespace (empty prefix). Set at the start of [`cascade_locked`]; read by
    /// `compound_matches` to resolve a selector's namespace component against the element's
    /// namespace. Empty when no `@namespace` rule is present (the common case), in which case
    /// namespace constraints are ignored and matching behaves exactly as before.
    pub(crate) static NAMESPACE_BINDINGS: std::cell::RefCell<NamespaceEnv> =
        const { std::cell::RefCell::new(NamespaceEnv { default_ns: None, prefixes: Vec::new() }) };

    /// The document's base URL, used to resolve relative `url(...)` references in **inline** styles
    /// (a `style="…"` attribute has no stylesheet, so its base is the document base — `<base href>`).
    /// Set via [`set_document_base_url`] before a cascade; stylesheet rules carry their own base.
    pub(crate) static DOCUMENT_BASE_URL: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Set the document base URL used to resolve relative `url(...)` in inline styles. Call before
/// [`cascade`] (mirrors [`set_viewport_metrics`]). `None` clears it (relative urls stay relative).
pub fn set_document_base_url(base: Option<&str>) {
    DOCUMENT_BASE_URL.with(|b| *b.borrow_mut() = base.map(str::to_string));
}

/// The resolved `@namespace` environment for one cascade.
#[derive(Default, Clone)]
pub(crate) struct NamespaceEnv {
    /// The default namespace URI (`@namespace url(...)` with no prefix), if any.
    pub(crate) default_ns: Option<String>,
    /// `(prefix, uri)` bindings from `@namespace prefix url(...)`.
    pub(crate) prefixes: Vec<(String, String)>,
}

impl NamespaceEnv {
    pub(crate) fn lookup(&self, prefix: &str) -> Option<&str> {
        self.prefixes
            .iter()
            .find(|(p, _)| p == prefix)
            .map(|(_, u)| u.as_str())
    }
}

/// Whether the root opted into a dark color scheme for this cascade (UA dark canvas + light text).
pub fn root_used_scheme_dark() -> bool {
    ROOT_USED_SCHEME_DARK.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn set_root_used_scheme_dark(dark: bool) {
    ROOT_USED_SCHEME_DARK.store(dark, std::sync::atomic::Ordering::Relaxed);
}

/// UA initial/default text color: black on a light page, light grey (`#e8e8e8`) when the root used
/// a dark color scheme. Read by `ComputedStyle::default()` (the cascade root's inherited color and
/// the `color: initial`/`unset` reset target), so dark pages get light text without per-element CSS.
pub(crate) fn ua_default_text_color() -> (u8, u8, u8) {
    if root_used_scheme_dark() {
        (0xe8, 0xe8, 0xe8)
    } else {
        (0, 0, 0)
    }
}

/// UA default canvas/page background: white on a light page, dark (`#1e1e1e`) when the root used a
/// dark color scheme. Read by the engine's `page_background` when no html/body `background-color`
/// is set.
pub fn ua_default_canvas_color() -> (u8, u8, u8) {
    if root_used_scheme_dark() {
        (0x1e, 0x1e, 0x1e)
    } else {
        (0xff, 0xff, 0xff)
    }
}

// Live pointer/keyboard interaction state used to evaluate `:hover`/`:focus`/`:active`/
// `:focus-within`/`:focus-visible` during the cascade. The engine sets these via
// `set_interaction_state` before each cascade. We store the hovered/focused node ids (the
// `usize` inside a `dom::NodeId`); `usize::MAX` means "none".
pub(crate) static HOVERED_NODE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(usize::MAX);
pub(crate) static FOCUSED_NODE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(usize::MAX);

/// Set the currently hovered and focused node ids (as the raw `usize` of their [`dom::NodeId`]),
/// or `None` for neither. Call before [`cascade`] whenever interaction state changes so
/// `:hover`/`:focus`/… re-evaluate. Mirrors [`set_viewport_metrics`].
pub fn set_interaction_state(hovered: Option<usize>, focused: Option<usize>) {
    use std::sync::atomic::Ordering;
    HOVERED_NODE.store(hovered.unwrap_or(usize::MAX), Ordering::Relaxed);
    FOCUSED_NODE.store(focused.unwrap_or(usize::MAX), Ordering::Relaxed);
}

pub(crate) fn interaction_hovered() -> Option<usize> {
    let v = HOVERED_NODE.load(std::sync::atomic::Ordering::Relaxed);
    if v == usize::MAX {
        None
    } else {
        Some(v)
    }
}
pub(crate) fn interaction_focused() -> Option<usize> {
    let v = FOCUSED_NODE.load(std::sync::atomic::Ordering::Relaxed);
    if v == usize::MAX {
        None
    } else {
        Some(v)
    }
}

pub(crate) fn viewport_width() -> f32 {
    let b = VIEWPORT_W_BITS.load(std::sync::atomic::Ordering::Relaxed);
    if b == 0 {
        1280.0
    } else {
        f32::from_bits(b)
    }
}
pub(crate) fn viewport_height() -> f32 {
    let b = VIEWPORT_H_BITS.load(std::sync::atomic::Ordering::Relaxed);
    if b == 0 {
        800.0
    } else {
        f32::from_bits(b)
    }
}
pub(crate) fn viewport_dpr() -> f32 {
    let b = VIEWPORT_DPR_BITS.load(std::sync::atomic::Ordering::Relaxed);
    if b == 0 {
        2.0
    } else {
        f32::from_bits(b)
    }
}

/// Viewport width (px) used for `min-width`/`max-width` media queries — the real window width.
pub(crate) fn assumed_viewport_width() -> f32 {
    viewport_width()
}
/// Viewport height (px) used to resolve `vh` units — the real window height.
pub(crate) fn assumed_viewport_height() -> f32 {
    viewport_height()
}
/// Width (px) used to evaluate `@container` conditions. Correct container sizing needs layout
/// (which runs after the cascade), so we approximate with the viewport width.
pub(crate) fn assumed_container_width() -> f32 {
    viewport_width()
}

/// Recursively compute styles. `parent` is the parent's computed style (the inheritance
/// source); `parent_vars` is the set of custom properties inherited from ancestors;
/// `parent_hidden` is true if any ancestor was `display: none`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cascade_node(
    doc: &dom::Document,
    id: dom::NodeId,
    parent: &ComputedStyle,
    parent_vars: &Arc<HashMap<String, String>>,
    parent_hidden: bool,
    index: &SelectorIndex,
    out: &mut HashMap<dom::NodeId, ComputedStyle>,
) {
    let node = doc.get(id);
    let (computed, vars) = if let dom::NodeData::Element(el) = &node.data {
        let (style, vars) =
            compute_element_style(doc, id, el, parent, parent_vars, parent_hidden, index);
        out.insert(id, style.clone());
        (style, vars)
    } else {
        // Non-elements inherit the parent style so text runs can read color/size off the
        // nearest element ancestor via the parent passed down.
        (parent.clone(), parent_vars.clone())
    };
    let hidden = parent_hidden || computed.display_none;
    for &child in &node.children {
        // Defensive: skip any child id that points outside the arena. The engine prunes these
        // after JS runs, but guarding here too means a stale id can never panic the renderer.
        if child.0 >= doc.len() {
            continue;
        }
        cascade_node(doc, child, &computed, &vars, hidden, index, out);
    }
}

/// Resolve one element's computed style: gather matching declarations from all origins in
/// precedence order, apply them, then layer inheritance.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_element_style<'a>(
    doc: &dom::Document,
    node_id: dom::NodeId,
    el: &dom::ElementData,
    parent: &ComputedStyle,
    parent_vars: &Arc<HashMap<String, String>>,
    parent_hidden: bool,
    index: &'a SelectorIndex<'a>,
) -> (ComputedStyle, Arc<HashMap<String, String>>) {
    // Start from inherited values; non-inherited properties get reset below.
    let mut style = ComputedStyle {
        custom_props: empty_vars(),
        direction: parent.direction,       // inherited
        writing_mode: parent.writing_mode, // inherited
        color: parent.color,
        background_color: None, // not inherited
        font_size: parent.font_size,
        font_family: parent.font_family.clone(),
        bold: parent.bold,
        italic: parent.italic,
        text_align: parent.text_align,
        display_none: false, // not inherited
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
        // Box properties are not inherited: each element starts from initial values.
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
        border_color: parent.color, // initial border-color is currentColor
        overflow_scrollport: false, // not inherited; initial overflow is `visible`
        // border-collapse / border-spacing inherit (set on the table, read by cells).
        border_collapse: parent.border_collapse,
        border_spacing: parent.border_spacing,
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
        // Typography extras inherit.
        line_height: parent.line_height,
        text_transform: parent.text_transform,
        letter_spacing: parent.letter_spacing,
        text_indent: parent.text_indent,
        white_space: parent.white_space,
        visibility: parent.visibility,
        list_style_type: parent.list_style_type,
        underline: parent.underline,
        line_through: parent.line_through,
        overline: parent.overline,
        // `vertical-align` is not inherited; each box starts at the baseline.
        vertical_align: VerticalAlign::Baseline,
        // Paint extras: opacity & border-radius are not inherited.
        opacity: 1.0,
        border_radius: 0.0,
        background_gradient: None,
        background_image_url: None,
        background_size: BgSize::Auto,
        background_repeat: BgRepeat::Repeat,
        background_position: (BgLen::Pct(0.0), BgLen::Pct(0.0)),
        box_shadows: Vec::new(),
        transform: None,
        transform_origin: (0.5, 0.5),
        // `mask-image` is not inherited; each box starts unmasked.
        mask_image: None,
        // `content` only applies to generated pseudo-elements; ordinary elements never carry one.
        content: None,
        before: None,
        after: None,
        // `color-scheme` inherits (initial `Normal`).
        color_scheme: parent.color_scheme,
    };
    if parent_hidden {
        style.display_none = true;
        style.display = Display::None;
    }

    // Collect (specificity, source_order, declarations) from every matching rule across all
    // origins. We process origins lowest-precedence-first and rely on a stable sort that puts
    // later, higher-specificity entries last so they win when applied in order.
    struct MatchEntry<'a> {
        origin: u8, // 0 = UA, 1 = presentational hints, 2 = author, 3 = inline
        specificity: u32,
        order: usize,
        decls: &'a [(String, String)],
        /// The owning sheet's base URL, for resolving relative `url(...)` values.
        base: Option<&'a str>,
    }
    let mut matches: Vec<MatchEntry> = Vec::new();

    // Gather only the rules that could match this element via the index, instead of scanning
    // every rule in every sheet. We dedup per rule (keyed by its unique global `order`),
    // keeping the MAX specificity across that rule's matching selectors — exactly what the
    // brute-force `rule_specificity` (max over comma selectors) produced.
    //
    // `best_by_order` maps a rule's `order` to its (origin, max-specificity, decls). A rule's
    // origin and decls are constant for a given order, so the only thing we fold is the max
    // specificity.
    let mut best_by_order: HashMap<usize, (u8, u32, &[(String, String)], Option<&'a str>)> =
        HashMap::new();
    // Matching `::before`/`::after` rules, kept separately so they cascade onto the pseudo style
    // rather than the element itself. Each is (origin, specificity, order, decls, base).
    let mut before_matches: Vec<(u8, u32, usize, &'a [(String, String)], Option<&'a str>)> =
        Vec::new();
    let mut after_matches: Vec<(u8, u32, usize, &'a [(String, String)], Option<&'a str>)> =
        Vec::new();
    let mut consider = |entry: &Entry<'a>| {
        // The compound must match the originating element either way; the pseudo just routes the
        // declarations to the element's ::before/::after style.
        if !complex_matches(doc, node_id, &entry.compiled.selector) {
            return;
        }
        match &entry.compiled.pseudo_element {
            Some(PseudoElement::Before) => {
                before_matches.push((
                    entry.origin,
                    entry.compiled.specificity,
                    entry.order,
                    entry.decls,
                    entry.base,
                ));
            }
            Some(PseudoElement::After) => {
                after_matches.push((
                    entry.origin,
                    entry.compiled.specificity,
                    entry.order,
                    entry.decls,
                    entry.base,
                ));
            }
            // Other pseudo-elements (`::marker`, `::highlight(x)`, …) don't generate layout boxes
            // here and don't apply to the originating element; they're resolved on demand by
            // `compute_pseudo_style` for `getComputedStyle`.
            Some(PseudoElement::Other(_)) => {}
            None => {
                best_by_order
                    .entry(entry.order)
                    .and_modify(|(_, spec, _, _)| *spec = (*spec).max(entry.compiled.specificity))
                    .or_insert((
                        entry.origin,
                        entry.compiled.specificity,
                        entry.decls,
                        entry.base,
                    ));
            }
        }
    };

    if let Some(id) = el.id() {
        if let Some(bucket) = index.by_id.get(id) {
            for e in bucket {
                consider(e);
            }
        }
    }
    for class in el.classes() {
        if let Some(bucket) = index.by_class.get(class) {
            for e in bucket {
                consider(e);
            }
        }
    }
    let tag_lower = el.tag.to_lowercase();
    if let Some(bucket) = index.by_type.get(&tag_lower) {
        for e in bucket {
            consider(e);
        }
    }
    for e in &index.universal {
        consider(e);
    }

    // Cascade-origin levels (sorted ascending, winner last): 0 = UA, 1 = presentational hints,
    // 2 = author, 3 = inline. The selector index tags UA entries 0 and author entries 1; remap
    // author to level 2 here so presentational hints (level 1) slot strictly between UA and author —
    // regardless of selector specificity (a UA `td { padding: 1px }` has specificity 1, but a hint
    // must still beat it for `cellpadding` to work, so origin level — not specificity — separates
    // them).
    for (order, (origin, specificity, decls, base)) in best_by_order {
        let level = if origin == 0 { 0 } else { 2 };
        matches.push(MatchEntry {
            origin: level,
            specificity,
            order,
            decls,
            base,
        });
    }

    // Presentational hints: HTML attributes (`border`, `bgcolor`, `align`, `width`, …) mapped to
    // CSS declarations at origin level 1 — ABOVE the UA stylesheet, BELOW all author CSS. See
    // `presentational_hints`.
    let hint_decls: Vec<(String, String)> = presentational_hints(doc, node_id, el);
    if !hint_decls.is_empty() {
        matches.push(MatchEntry {
            origin: 1,
            specificity: 0,
            order: usize::MAX - 1,
            decls: &hint_decls,
            base: None,
        });
    }

    // Inline style is its own origin (level 3) with highest precedence.
    let inline_decls: Vec<(String, String)> = el
        .attrs
        .get("style")
        .map(|s| css::parse_declarations(s))
        .unwrap_or_default();
    // Inline `style=""` url()s resolve against the document base (a `style` attribute has no
    // stylesheet). Held in a local so the borrow lives across the apply loop below.
    let inline_base = DOCUMENT_BASE_URL.with(|b| b.borrow().clone());
    if !inline_decls.is_empty() {
        // Inline is the sole top-level entry; the sort tiebreaks on `order` only within the
        // same origin/specificity, so the exact value is immaterial. Use MAX to keep the
        // "applied last" intent explicit.
        matches.push(MatchEntry {
            origin: 3,
            specificity: 0,
            order: usize::MAX,
            decls: &inline_decls,
            base: inline_base.as_deref(),
        });
    }

    // Sort by (origin, specificity, order) ascending so the winner is applied last.
    matches.sort_by(|a, b| {
        a.origin
            .cmp(&b.origin)
            .then(a.specificity.cmp(&b.specificity))
            .then(a.order.cmp(&b.order))
    });

    // Build this element's custom-property environment: inherit the ancestors' vars, then
    // override with any `--name: value` declared on this element (in cascade order, so the
    // winning declaration applies last).
    //
    // Copy-on-write: custom properties inherit, and on token-heavy sites the inherited set is
    // large (hundreds of entries). The vast majority of elements declare none of their own, so
    // unless this element either declares a `--var` OR there are `@property` registrations that may
    // reset/seed the environment, we share the parent's `Arc` untouched instead of deep-cloning it.
    let declares_var = matches
        .iter()
        .any(|m| m.decls.iter().any(|(prop, _)| prop.starts_with("--")));
    let has_registered = REGISTERED_PROPERTIES.with(|c| !c.borrow().is_empty());
    let vars: Arc<HashMap<String, String>> = if !declares_var && !has_registered {
        Arc::clone(parent_vars)
    } else {
        let mut vars = (**parent_vars).clone();
        let mut declared_here: std::collections::HashSet<String> = std::collections::HashSet::new();
        for m in &matches {
            for (prop, val) in m.decls {
                if let Some(name) = prop.strip_prefix("--") {
                    let key = format!("--{name}");
                    declared_here.insert(key.clone());
                    vars.insert(key, val.clone());
                }
            }
        }
        // Apply `@property` registrations to the custom-property environment. A registered
        // non-inherited property that this element did NOT declare resets to its initial value
        // (it does not inherit). Any registered property with an initial value that is still
        // absent is seeded with that initial value (so it appears in the computed-style
        // enumeration on every element). `*`-syntax registrations without an initial value are
        // not seeded.
        REGISTERED_PROPERTIES.with(|c| {
            for pr in c.borrow().iter() {
                if !pr.inherits && !declared_here.contains(&pr.name) {
                    if let Some(iv) = &pr.initial_value {
                        vars.insert(pr.name.clone(), iv.clone());
                    } else {
                        // Non-inherited, no initial value: it must not carry an inherited value.
                        vars.remove(&pr.name);
                    }
                } else if !vars.contains_key(&pr.name) {
                    if let Some(iv) = &pr.initial_value {
                        vars.insert(pr.name.clone(), iv.clone());
                    }
                }
            }
        });
        Arc::new(vars)
    };

    // Now apply the regular declarations, resolving any `var(...)` references against `vars`
    // and supplying the current/inherited color for `currentColor`/`inherit`.
    let inherited_color = parent.color;
    // `font-size` must be resolved before any other declaration, because `em`-based values
    // (insets, line-height, edges…) compute against *this element's* font size regardless of
    // declaration order. Apply the winning `font-size` first, then everything else.
    for m in &matches {
        for (prop, val) in m.decls {
            if prop.eq_ignore_ascii_case("font-size") {
                let (val, _imp) = split_importance(val);
                let resolved = resolve_vars(val, &vars);
                let current_color = style.color;
                apply_declaration(
                    &mut style,
                    prop,
                    &resolved,
                    parent,
                    current_color,
                    inherited_color,
                    m.base,
                );
            }
        }
    }
    // Normal (non-important) declarations, in ascending cascade order (later wins).
    for m in &matches {
        for (prop, val) in m.decls {
            if prop.starts_with("--") || prop.eq_ignore_ascii_case("font-size") {
                continue; // custom properties are environment; font-size already applied above
            }
            let (val, important) = split_importance(val);
            if important {
                continue; // important declarations are applied in the final pass below
            }
            let resolved = resolve_vars(val, &vars);
            let current_color = style.color;
            apply_declaration(
                &mut style,
                prop,
                &resolved,
                parent,
                current_color,
                inherited_color,
                m.base,
            );
        }
    }
    // `!important` declarations win over all normal ones: apply them last, still in ascending
    // cascade order so the most-specific/last important declaration takes effect.
    for m in &matches {
        for (prop, val) in m.decls {
            if prop.starts_with("--") {
                continue;
            }
            let (val, important) = split_importance(val);
            if !important {
                continue;
            }
            let resolved = resolve_vars(val, &vars);
            let current_color = style.color;
            apply_declaration(
                &mut style,
                prop,
                &resolved,
                parent,
                current_color,
                inherited_color,
                m.base,
            );
        }
    }

    // The UA stylesheet emits `display: block` for block tags; everything else defaults to
    // inline. If no author/UA rule set a display, fall back to the per-tag default.
    let display_was_set = matches.iter().any(|m| {
        m.decls
            .iter()
            .any(|(p, _)| p.eq_ignore_ascii_case("display"))
    });
    if !display_was_set && style.display == Display::Inline && is_block_tag(&el.tag) {
        style.display = Display::Block;
    }
    // A floated box is blockified (CSS 2.2 §9.7): a non-`none` `float` forces inline-level displays
    // to block so it lays out as a block-level float. (Absolute/fixed override float, handled below.)
    if style.float != Float::None && matches!(style.display, Display::Inline | Display::InlineBlock)
    {
        style.display = Display::Block;
    }
    // `float` has no effect on absolutely positioned boxes (CSS 2.2 §9.7): `position:absolute`/
    // `fixed` wins and the box is taken out of flow, not floated.
    if matches!(style.position, Position::Absolute | Position::Fixed) {
        style.float = Float::None;
    }
    if parent_hidden {
        style.display = Display::None;
    }

    // Keep the legacy derived flags in sync for existing readers (engine / layout fallbacks).
    style.display_none = style.display == Display::None;
    style.display_block = matches!(
        style.display,
        Display::Block | Display::Flex | Display::Grid | Display::None
    );

    // Cascade ::before / ::after styles. Each inherits from this element's computed style, then
    // applies its own matching rules. A pseudo-element only generates a box when its `content`
    // resolves to Some, so we keep the result only in that case.
    style.before = cascade_pseudo(&style, el, &before_matches, &vars);
    style.after = cascade_pseudo(&style, el, &after_matches, &vars);

    // Expose the resolved custom-property environment for CSSOM reads. Custom props inherit, so
    // this includes ancestor-declared vars (the *computed* value, per spec).
    style.custom_props = vars.clone();

    (style, vars)
}

/// Cascade a `::before`/`::after` pseudo-element's style from its originating element's computed
/// style (the inheritance source) plus the `matches` (origin, specificity, order, decls) rules
/// whose compound matched. Returns the boxed pseudo style only when `content` resolved to Some
/// (a pseudo with no `content` generates no box, per spec). `vars` is the element's custom-property
/// environment (pseudo-elements inherit it).
pub(crate) fn cascade_pseudo(
    element_style: &ComputedStyle,
    el: &dom::ElementData,
    matches: &[(u8, u32, usize, &[(String, String)], Option<&str>)],
    vars: &HashMap<String, String>,
) -> Option<Box<ComputedStyle>> {
    if matches.is_empty() {
        return None;
    }
    // Start from values inherited from the originating element (a fresh element-style snapshot,
    // already carrying the element's inherited typography/color), but reset the non-inherited
    // box/content fields to initial.
    let mut ps = element_style.clone();
    ps.background_color = None;
    ps.background_gradient = None;
    ps.mask_image = None;
    ps.box_shadows = Vec::new();
    ps.transform = None;
    ps.transform_origin = (0.5, 0.5);
    ps.margin = Edges::default();
    ps.padding = Edges::default();
    ps.border = Edges::default();
    ps.border_color = element_style.color;
    ps.width = None;
    ps.height = None;
    ps.min_width = None;
    ps.max_width = None;
    ps.min_height = None;
    ps.max_height = None;
    ps.position = Position::Static;
    ps.top = None;
    ps.right = None;
    ps.bottom = None;
    ps.left = None;
    ps.z_index = None;
    ps.opacity = 1.0;
    ps.border_radius = 0.0;
    ps.display = Display::Inline; // generated content is inline by default
    ps.display_block = false;
    ps.content = None;
    ps.before = None;
    ps.after = None;

    // Apply matching rules in cascade order (origin, specificity, source order ascending → winner
    // last). The inheritance source for `currentColor`/`inherit` is the originating element.
    let mut sorted: Vec<&(u8, u32, usize, &[(String, String)], Option<&str>)> =
        matches.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let inherited_color = element_style.color;
    for (_, _, _, decls, base) in sorted {
        for (prop, val) in *decls {
            if prop.starts_with("--") {
                continue;
            }
            let resolved = resolve_vars(val, vars);
            let current_color = ps.color;
            apply_declaration(
                &mut ps,
                prop,
                &resolved,
                element_style,
                current_color,
                inherited_color,
                *base,
            );
        }
    }

    // No `content` (or `content: none`) → no generated box.
    let content = ps.content.take()?;
    // Resolve `attr(name)` now that we have the element.
    ps.content = Some(resolve_content_attr(&content, el));
    // Keep derived display flags consistent for downstream readers.
    ps.display_none = ps.display == Display::None;
    ps.display_block = matches!(
        ps.display,
        Display::Block | Display::Flex | Display::Grid | Display::None
    );
    Some(Box::new(ps))
}

/// Gather the candidate index entries for `el` (its id/class/type buckets + the universal bucket),
/// the same set `cascade_node` considers. Returned in bucket order; callers filter + sort.
pub(crate) fn candidate_entries<'i, 'a>(
    index: &'i SelectorIndex<'a>,
    el: &dom::ElementData,
) -> Vec<&'i Entry<'a>> {
    let mut out: Vec<&'i Entry<'a>> = Vec::new();
    if let Some(id) = el.id() {
        if let Some(bucket) = index.by_id.get(id) {
            out.extend(bucket.iter());
        }
    }
    for class in el.classes() {
        if let Some(bucket) = index.by_class.get(class) {
            out.extend(bucket.iter());
        }
    }
    let tag_lower = el.tag.to_lowercase();
    if let Some(bucket) = index.by_type.get(&tag_lower) {
        out.extend(bucket.iter());
    }
    out.extend(index.universal.iter());
    out
}

/// Compute the cascaded computed style of a pseudo-element of `node_id`, for `getComputedStyle`.
///
/// `element_style` is the originating element's already-cascaded style (the inheritance source).
/// `pseudo_key` is the canonical key from [`parse_gcs_pseudo`] (`"before"`, `"marker"`,
/// `"highlight(x)"`, …). Returns `None` only if `node_id` isn't an element; otherwise it always
/// returns a (possibly rule-less, but non-empty) pseudo style — matching browsers, which expose a
/// full computed style for any tree-abiding pseudo-element of any element.
///
/// Box / non-inherited properties start at their initial values (the pseudo is a fresh box that
/// merely inherits typography/color from the originating element); matching author + UA rules for
/// that pseudo then cascade on top. `content` is *not* required (unlike layout box generation) —
/// `getComputedStyle(el, "::before")` reports a style even when there's no generated box.
pub fn compute_pseudo_style(
    doc: &dom::Document,
    sheets: &[css::Stylesheet],
    node_id: dom::NodeId,
    element_style: &ComputedStyle,
    pseudo_key: &str,
) -> Option<ComputedStyle> {
    let el = el_of(doc, node_id)?.clone();
    let ua = user_agent_stylesheet();
    let index = SelectorIndex::build(&ua, sheets);

    // Collect every rule whose compound matches the originating element AND whose pseudo-element
    // equals the requested key. Mirror `cascade_node`'s bucketed lookup.
    let mut matches: Vec<(u8, u32, usize, &[(String, String)], Option<&str>)> = Vec::new();
    for entry in candidate_entries(&index, &el) {
        if !matches!(&entry.compiled.pseudo_element, Some(pe) if pe.key() == pseudo_key) {
            continue;
        }
        if !complex_matches(doc, node_id, &entry.compiled.selector) {
            continue;
        }
        let origin = if entry.origin == 0 { 0 } else { 2 };
        matches.push((
            origin,
            entry.compiled.specificity,
            entry.order,
            entry.decls,
            entry.base,
        ));
    }

    // Inherit typography/color from the originating element, then reset the box / non-inherited
    // fields to their initial values (same reset list as `cascade_pseudo`).
    let mut ps = element_style.clone();
    ps.background_color = None;
    ps.background_gradient = None;
    ps.mask_image = None;
    ps.box_shadows = Vec::new();
    ps.transform = None;
    ps.transform_origin = (0.5, 0.5);
    ps.margin = Edges::default();
    ps.padding = Edges::default();
    ps.border = Edges::default();
    ps.border_color = element_style.color;
    ps.width = None;
    ps.height = None;
    ps.min_width = None;
    ps.max_width = None;
    ps.min_height = None;
    ps.max_height = None;
    ps.position = Position::Static;
    ps.top = None;
    ps.right = None;
    ps.bottom = None;
    ps.left = None;
    ps.z_index = None;
    ps.opacity = 1.0;
    ps.border_radius = 0.0;
    ps.display = Display::Inline; // generated content is inline by default
    ps.display_block = false;
    ps.display_none = false;
    ps.content = None;
    ps.before = None;
    ps.after = None;

    // The originating element's custom-property environment is inherited by its pseudos. Rebuild it
    // here from the element's matching declarations (the cascade doesn't expose the stored map).
    let vars = element_vars(doc, node_id, &el, &index);

    matches.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let inherited_color = element_style.color;
    // `parse_length` drops percentage width/height (it has no basis at cascade time). For a
    // pseudo-element the containing block IS the originating element's box, which we know here, so
    // track the winning percentage and resolve it against the element's content extents.
    let mut width_pct: Option<f32> = None;
    let mut height_pct: Option<f32> = None;
    for (_, _, _, decls, base) in &matches {
        for (prop, val) in *decls {
            if prop.starts_with("--") {
                continue;
            }
            let resolved = resolve_vars(val, &vars);
            let current_color = ps.color;
            match prop.as_str() {
                "width" => width_pct = parse_percent(&resolved),
                "height" => height_pct = parse_percent(&resolved),
                _ => {}
            }
            apply_declaration(
                &mut ps,
                prop,
                &resolved,
                element_style,
                current_color,
                inherited_color,
                *base,
            );
        }
    }
    // Resolve a tracked percentage width/height against the originating element's content box. Only
    // when `apply_declaration` left the field as `None` (i.e. it was a percentage it couldn't store).
    if let Some(p) = width_pct {
        if ps.width.is_none() {
            if let Some(basis) = element_style.width {
                ps.width = Some(p / 100.0 * basis);
            }
        }
    }
    if let Some(p) = height_pct {
        if ps.height.is_none() {
            if let Some(basis) = element_style.height {
                ps.height = Some(p / 100.0 * basis);
            }
        }
    }

    // Resolve `content`'s `attr()` now that we have the element (if any content was set).
    if let Some(content) = ps.content.take() {
        ps.content = Some(resolve_content_attr(&content, &el));
    }

    // Item-based blockification: a pseudo-element child of a flex/grid container is blockified.
    if matches!(element_style.display, Display::Flex | Display::Grid)
        && matches!(ps.display, Display::Inline)
    {
        ps.display = Display::Block;
    }

    // Keep derived display flags consistent for downstream readers.
    ps.display_none = ps.display == Display::None;
    ps.display_block = matches!(
        ps.display,
        Display::Block | Display::Flex | Display::Grid | Display::None
    );
    Some(ps)
}
