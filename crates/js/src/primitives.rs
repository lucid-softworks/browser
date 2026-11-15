use crate::*;

/// Render a JavaScript value, falling back to "undefined" when coercion throws.
pub(crate) fn render_value(scope: &mut Ctx, value: Value) -> String {
    scope
        .coerce_string(&value)
        .map(|s| decode_lumen_string(&s))
        .unwrap_or_else(|_| "undefined".to_string())
}

pub(crate) fn arg_str(scope: &mut Ctx, args: &NativeArgs, i: i32) -> String {
    if i >= args.length() {
        return String::new();
    }
    render_value(scope, args.get(i))
}

pub(crate) fn arg_node(scope: &mut Ctx, args: &NativeArgs, i: i32) -> Option<dom::NodeId> {
    if i >= args.length() {
        return None;
    }
    let n = scope.coerce_number(&args.get(i)).ok()?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    let id = dom::NodeId(n as usize);
    if id.0 >= host_state(scope).doc.borrow().len() {
        return None;
    }
    Some(id)
}

// Lumen represents lone UTF-16 surrogate units using the private-use scalar range
// U+10F800..U+10FFFF. Real scalars in that range are represented as two encoded units.
// Decode at the Rust boundary exactly as UTF-16 lossy conversion did for the old VM.
fn decode_lumen_string(s: &str) -> String {
    if !s
        .chars()
        .any(|c| (0x10f800..=0x10ffff).contains(&(c as u32)))
    {
        return s.to_string();
    }
    let mut units = Vec::with_capacity(s.len());
    for c in s.chars() {
        if (0x10f800..=0x10ffff).contains(&(c as u32)) {
            units.push((c as u32 - 0x10f800 + 0xd800) as u16);
        } else {
            let mut pair = [0; 2];
            units.extend_from_slice(c.encode_utf16(&mut pair));
        }
    }
    String::from_utf16_lossy(&units)
}

fn encode_lumen_string(s: &str) -> String {
    let mut encoded = String::with_capacity(s.len());
    for c in s.chars() {
        if (0x10f800..=0x10ffff).contains(&(c as u32)) {
            let mut pair = [0; 2];
            for &unit in c.encode_utf16(&mut pair).iter() {
                encoded.push(char::from_u32(0x10f800 + u32::from(unit) - 0xd800).unwrap());
            }
        } else {
            encoded.push(c);
        }
    }
    encoded
}

pub(crate) fn js_str(_scope: &mut Ctx, s: &str) -> Value {
    Value::from_string(encode_lumen_string(s))
}

/// Construct arrays through the realm's captured intrinsic constructor, never page replacements.
fn js_array(scope: &mut Ctx, elements: Vec<Value>) -> Value {
    let global = scope.global_this();
    let ctor = scope
        .realm_value_for_host(&global, "browser.array")
        .expect("Array intrinsic");
    let array = scope
        .construct_value(ctor, &[Value::Num(elements.len() as f64)])
        .unwrap_or_else(|_| panic!("Array construction"));
    for (i, value) in elements.into_iter().enumerate() {
        let _ = scope.member_set(&array, &i.to_string(), value);
    }
    array
}

pub(crate) fn js_id_array(scope: &mut Ctx, ids: &[dom::NodeId]) -> Value {
    js_array(
        scope,
        ids.iter().map(|id| Value::Num(id.0 as f64)).collect(),
    )
}

pub(crate) fn js_str_array(scope: &mut Ctx, items: &[String]) -> Value {
    js_array(
        scope,
        items
            .iter()
            .map(|s| Value::from_string(encode_lumen_string(s)))
            .collect(),
    )
}

// ---------------------------------------------------------------------------------------------
// Native primitive callbacks. These are bare functions (callbacks receive the realm context); they
// recover the shared DOM + console from the context slot via `host_state(scope)`. The JS
// bootstrap (DOCUMENT_BOOTSTRAP) builds `document`/element objects on top of these.
// ---------------------------------------------------------------------------------------------

/// `__consoleLog(...args)` — push a space-joined line into the shared console buffer.
pub(crate) fn prim_console_log(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let mut parts = Vec::with_capacity(args.length() as usize);
    for i in 0..args.length() {
        let v = args.get(i);
        parts.push(render_value(scope, v));
    }
    let line = parts.join(" ");
    host_state(scope).console.borrow_mut().push(line);
}

/// `__createElement(tag) -> id`
pub(crate) fn prim_create_element(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let tag = arg_str(scope, &args, 0);
    let state = host_state(scope);
    let id = state.doc.borrow_mut().alloc(
        dom::NodeData::Element(dom::ElementData {
            tag,
            attrs: Default::default(),
            namespace: None,
        }),
        None,
    );
    rv.set_double(id.0 as f64);
}

/// `__createText(text) -> id`
pub(crate) fn prim_create_text(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let text = arg_str(scope, &args, 0);
    let state = host_state(scope);
    let id = state
        .doc
        .borrow_mut()
        .alloc(dom::NodeData::Text(text), None);
    rv.set_double(id.0 as f64);
}

/// `__createComment(text) -> id`
pub(crate) fn prim_create_comment(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let text = arg_str(scope, &args, 0);
    let state = host_state(scope);
    let id = state
        .doc
        .borrow_mut()
        .alloc(dom::NodeData::Comment(text), None);
    rv.set_double(id.0 as f64);
}

/// `__createCData(text) -> id` — a parentless CDATASection node.
pub(crate) fn prim_create_cdata(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let text = arg_str(scope, &args, 0);
    let state = host_state(scope);
    let id = state
        .doc
        .borrow_mut()
        .alloc(dom::NodeData::Cdata(text), None);
    rv.set_double(id.0 as f64);
}

/// `__createDocumentNode() -> id` — a fresh, detached `Document` arena node (nodeType 9). Backs the
/// off-document documents produced by `new Document()` / `DOMImplementation.create{HTML,}Document`,
/// so they have a real tree (appendChild / childNodes / traversal all work).
pub(crate) fn prim_create_document_node(scope: &mut Ctx, _args: NativeArgs, rv: &mut NativeReturn) {
    let state = host_state(scope);
    let id = state.doc.borrow_mut().alloc(dom::NodeData::Document, None);
    rv.set_double(id.0 as f64);
}

/// `__createDocumentFragment() -> id` — a parentless `DocumentFragment` arena node.
pub(crate) fn prim_create_document_fragment(
    scope: &mut Ctx,
    _args: NativeArgs,
    rv: &mut NativeReturn,
) {
    let state = host_state(scope);
    let id = state
        .doc
        .borrow_mut()
        .alloc(dom::NodeData::DocumentFragment, None);
    rv.set_double(id.0 as f64);
}

/// `__createDocumentType(name, publicId, systemId) -> id` — a parentless `DocumentType` node.
pub(crate) fn prim_create_document_type(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let name = arg_str(scope, &args, 0);
    let public_id = arg_str(scope, &args, 1);
    let system_id = arg_str(scope, &args, 2);
    let state = host_state(scope);
    let id = state.doc.borrow_mut().alloc(
        dom::NodeData::DocumentType(dom::DoctypeData {
            name,
            public_id,
            system_id,
        }),
        None,
    );
    rv.set_double(id.0 as f64);
}

/// `__createProcessingInstruction(target, data) -> id` — a parentless PI node.
pub(crate) fn prim_create_processing_instruction(
    scope: &mut Ctx,
    args: NativeArgs,
    rv: &mut NativeReturn,
) {
    let target = arg_str(scope, &args, 0);
    let data = arg_str(scope, &args, 1);
    let state = host_state(scope);
    let id = state.doc.borrow_mut().alloc(
        dom::NodeData::ProcessingInstruction(dom::ProcessingInstructionData { target, data }),
        None,
    );
    rv.set_double(id.0 as f64);
}

/// `__doctypeInfo(id) -> { name, publicId, systemId } | null` for a DocumentType node.
pub(crate) fn prim_doctype_info(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let info = node.and_then(|n| match &state.doc.borrow().get(n).data {
        dom::NodeData::DocumentType(d) => {
            Some((d.name.clone(), d.public_id.clone(), d.system_id.clone()))
        }
        _ => None,
    });
    match info {
        Some((name, public_id, system_id)) => {
            let obj = Value::Obj(scope.new_object());
            let k = "name";
            let v = js_str(scope, &name);
            let _ = scope.member_set(&obj, k, v);
            let k = "publicId";
            let v = js_str(scope, &public_id);
            let _ = scope.member_set(&obj, k, v);
            let k = "systemId";
            let v = js_str(scope, &system_id);
            let _ = scope.member_set(&obj, k, v);
            rv.set(obj);
        }
        None => rv.set_null(),
    }
}

/// `__piTarget(id) -> string | null` — a ProcessingInstruction node's target.
pub(crate) fn prim_pi_target(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let target = node.and_then(|n| match &state.doc.borrow().get(n).data {
        dom::NodeData::ProcessingInstruction(p) => Some(p.target.clone()),
        _ => None,
    });
    match target {
        Some(t) => {
            let s = js_str(scope, &t);
            rv.set(s);
        }
        None => rv.set_null(),
    }
}

/// Deep/shallow clone of `id` in the arena. The clone is parentless. Element attributes are copied;
/// with `deep`, children are recursively cloned and appended. Returns the new node id (or the
/// original `id` if it's out of range). `__nsMeta` is copied JS-side by the wrapper.
pub(crate) fn clone_node_arena(
    doc: &mut dom::Document,
    id: dom::NodeId,
    deep: bool,
) -> dom::NodeId {
    let data = doc.get(id).data.clone();
    let new_id = doc.alloc(data, None);
    if deep {
        let kids = doc.get(id).children.clone();
        for child in kids {
            let cloned = clone_node_arena(doc, child, true);
            doc.get_mut(cloned).parent = Some(new_id);
            doc.get_mut(new_id).children.push(cloned);
        }
    }
    new_id
}

/// `__cloneNode(id, deep) -> id` — clone an arena node (see [`clone_node_arena`]).
pub(crate) fn prim_clone_node(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let deep = args.get(1).boolean_value(scope);
    let state = host_state(scope);
    match node {
        Some(n) => {
            let new_id = clone_node_arena(&mut state.doc.borrow_mut(), n, deep);
            rv.set_double(new_id.0 as f64);
        }
        None => rv.set_double(-1.0),
    }
}

/// `__getAttr(id, name) -> string | null`
pub(crate) fn prim_get_attr(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let name = arg_str(scope, &args, 1);
    let state = host_state(scope);
    let val = node.and_then(|n| match &state.doc.borrow().get(n).data {
        dom::NodeData::Element(e) => e.attrs.get(&name).cloned(),
        _ => None,
    });
    match val {
        Some(v) => {
            let s = js_str(scope, &v);
            rv.set(s);
        }
        None => rv.set_null(),
    }
}

// ---------------------------------------------------------------------------------------------
// getComputedStyle support (computed in-Session).
//
// The Session's JS runs on its own worker thread while the engine is blocked, and most
// feature-detection (browserscore.dev etc.) reads `getComputedStyle(probe).someProp` during init,
// before any layout exists — so we cannot call back into the engine for the cascade. Instead we run
// the *same* `style::cascade` here over the live DOM, caching it and invalidating on every DOM
// mutation (via `dom_version`).
//
// Author sheets include inline `<style>` blocks and enabled external stylesheet links, fetched
// through the host capability using the document's base URL. Each sheet retains its own base for
// relative CSS resources. The cascade also adds the UA sheet and inline style attributes.
// ---------------------------------------------------------------------------------------------

/// Join `href` onto `base` (both treated as URLs); returns `base` if either fails to parse.
pub(crate) fn join_url(base: &str, href: &str) -> String {
    wurl::resolve(href.trim(), base).unwrap_or_else(|| base.to_string())
}

/// The document's base URL: the first `<base href>` resolved against the page URL, else the page URL.
pub(crate) fn document_base_url(doc: &dom::Document, page_url: &str) -> String {
    fn find(doc: &dom::Document, id: dom::NodeId) -> Option<String> {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag.eq_ignore_ascii_case("base") {
                if let Some(h) = e.attrs.get("href") {
                    if !h.trim().is_empty() {
                        return Some(h.trim().to_string());
                    }
                }
            }
        }
        for &c in &doc.get(id).children {
            if let Some(h) = find(doc, c) {
                return Some(h);
            }
        }
        None
    }
    match find(doc, doc.root()) {
        Some(href) if !page_url.is_empty() => join_url(page_url, &href),
        _ => page_url.to_string(),
    }
}

/// Collect applicable inline and linked author stylesheets in document order, preserving bases.
pub(crate) fn collect_author_sheets(
    doc: &dom::Document,
    fetcher: &Rc<dyn Fn(&str) -> Option<(String, String)>>,
    page_url: &str,
) -> Vec<css::Stylesheet> {
    // Resolve the document base URL (honoring `<base href>`) and publish it so the cascade resolves
    // relative `url(...)` in inline `style=""` attributes (which have no stylesheet of their own).
    let doc_base = document_base_url(doc, page_url);
    style::set_document_base_url(if doc_base.is_empty() {
        None
    } else {
        Some(&doc_base)
    });

    /// One author stylesheet in document order. `seq` is the node id (arena nodes are allocated in
    /// creation order), which determines the preferred set independent of final tree order. `base` is
    /// the URL relative `url(...)`s in this sheet resolve against (the sheet's own URL).
    struct SheetEntry {
        title: Option<String>,
        alternate: bool,
        css: String,
        seq: usize,
        base: String,
    }
    fn walk(
        doc: &dom::Document,
        id: dom::NodeId,
        out: &mut Vec<SheetEntry>,
        fetcher: &Rc<dyn Fn(&str) -> Option<(String, String)>>,
        doc_base: &str,
    ) {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag.eq_ignore_ascii_case("style") {
                // An adopted-stylesheet mirror carries its constructed sheet's base via
                // `data-base-url`; a normal <style>'s rules resolve against the document base.
                let base = e
                    .attrs
                    .get("data-base-url")
                    .filter(|b| !b.is_empty())
                    .cloned()
                    .unwrap_or_else(|| doc_base.to_string());
                out.push(SheetEntry {
                    title: e.attrs.get("title").cloned(),
                    alternate: false,
                    css: text_content(doc, id),
                    seq: id.0,
                    base,
                });
                return; // don't descend into a <style>'s text as if it were markup
            }
            if e.tag.eq_ignore_ascii_case("link") {
                // An enabled (no `disabled` attribute) stylesheet `<link>` contributes its fetched CSS
                // to the cascade so `getComputedStyle` reflects `<link>` styles. `data:` URLs are
                // decoded inline; others go through the host fetcher (warmed by the cascade fetch).
                let rels: Vec<&str> = e
                    .attrs
                    .get("rel")
                    .map(|s| s.as_str())
                    .unwrap_or("")
                    .split_whitespace()
                    .collect();
                let is_stylesheet = rels.iter().any(|r| r.eq_ignore_ascii_case("stylesheet"));
                let is_alternate = rels.iter().any(|r| r.eq_ignore_ascii_case("alternate"));
                let disabled = e.attrs.contains_key("disabled");
                if is_stylesheet && !disabled {
                    if let Some(href) = e.attrs.get("href") {
                        let resolved = join_url(doc_base, href);
                        if let Some(css) = fetch_link_css(&resolved, fetcher) {
                            // The link sheet's rules resolve against the link's own (absolute) URL.
                            out.push(SheetEntry {
                                title: e.attrs.get("title").cloned(),
                                alternate: is_alternate,
                                css,
                                seq: id.0,
                                base: resolved,
                            });
                        }
                    }
                }
                return;
            }
        }
        for &child in &doc.get(id).children {
            walk(doc, child, out, fetcher, doc_base);
        }
    }
    let mut entries: Vec<SheetEntry> = Vec::new();
    walk(doc, doc.root(), &mut entries, fetcher, &doc_base);

    // The "preferred style sheet set" name: the title of the first non-alternate sheet with a
    // non-empty title. Sheets with no/empty title are persistent (always apply); a non-empty title
    // names a set, and only the preferred set's sheets apply by default. Alternates we treat as
    // enabled when present (the DOM doesn't tell us which set, if any, the user selected).
    let preferred: Option<&str> = entries
        .iter()
        .filter(|e| !e.alternate && e.title.as_deref().map(|t| !t.is_empty()).unwrap_or(false))
        .min_by_key(|e| e.seq)
        .and_then(|e| e.title.as_deref());

    // Parse each applicable sheet against its own base, in document order (the cascade applies sheets
    // in order, so per-sheet parsing preserves the previous concatenated order while keeping bases).
    let mut sheets: Vec<css::Stylesheet> = Vec::new();
    for e in &entries {
        let applies = e.alternate
            || match e.title.as_deref() {
                None | Some("") => true,
                Some(t) => Some(t) == preferred,
            };
        if applies && !e.css.trim().is_empty() {
            if e.base.is_empty() {
                sheets.push(css::parse(&e.css));
            } else {
                sheets.push(css::parse_with_base(&e.css, &e.base));
            }
        }
    }
    sheets
}

/// Fetch the CSS text behind a stylesheet `<link href>`. `data:` URLs are decoded directly (the
/// `text/css` body after the comma, percent-decoded); anything else goes through the host GET fetcher.
pub(crate) fn fetch_link_css(
    href: &str,
    fetcher: &Rc<dyn Fn(&str) -> Option<(String, String)>>,
) -> Option<String> {
    if let Some(rest) = href.strip_prefix("data:") {
        let comma = rest.find(',')?;
        let (meta, data) = (&rest[..comma], &rest[comma + 1..]);
        if meta.trim_end().ends_with(";base64") {
            return None; // base64 data: stylesheets are rare; not decoded here
        }
        return Some(percent_decode_str(data));
    }
    let (body, ctype) = fetcher(href)?;
    // A linked stylesheet whose response declares a non-`text/css` type isn't a CSS resource and
    // must not be applied (HTML "obtain a CSS style sheet" / CORB). An absent type stays lenient.
    let essence = ctype
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if essence.is_empty() || essence == "text/css" {
        Some(body)
    } else {
        None
    }
}

/// Minimal percent-decoding for `data:` URL bodies (`%XX` → byte; other chars pass through).
pub(crate) fn percent_decode_str(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Directory where `localStorage` buckets persist (one JSON file per origin).
pub(crate) fn storage_dir() -> std::path::PathBuf {
    let base = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(".imlunahey-browser").join("localstorage")
}

/// Map a storage key (an origin like `https://example.com`) to a safe filename.
pub(crate) fn storage_path(key: &str) -> std::path::PathBuf {
    let safe: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = if safe.is_empty() {
        "default".to_string()
    } else {
        safe
    };
    storage_dir().join(format!("{}.json", &safe[..safe.len().min(180)]))
}

/// `__storageLoad(key) -> string` — the persisted JSON for `key`, or `""`.
pub(crate) fn prim_storage_load(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let key = arg_str(scope, &args, 0);
    let content = std::fs::read_to_string(storage_path(&key)).unwrap_or_default();
    let s = js_str(scope, &content);
    rv.set(s);
}

/// `__storageSave(key, json)` — persist `json` for `key` (localStorage write-through).
pub(crate) fn prim_storage_save(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let key = arg_str(scope, &args, 0);
    let json = arg_str(scope, &args, 1);
    let _ = std::fs::create_dir_all(storage_dir());
    let _ = std::fs::write(storage_path(&key), json);
}

/// `__cookie()` -> string — current document.cookie value for the page URL (from the host jar).
pub(crate) fn prim_cookie(scope: &mut Ctx, _args: NativeArgs, rv: &mut NativeReturn) {
    let state = host_state(scope);
    let url = state.page_url.borrow().clone();
    let s = (state.cookie_getter)(&url);
    rv.set(js_str(scope, &s));
}

/// `__setCookie(v)` — set a cookie string (from document.cookie = "...").
pub(crate) fn prim_set_cookie(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let v = arg_str(scope, &args, 0);
    let state = host_state(scope);
    let url = state.page_url.borrow().clone();
    let _ = (state.cookie_setter)(&url, &v);
}

/// `__scrollY() -> number` — the page's current vertical scroll offset (CSS px), so `window.scrollY`
/// / `pageYOffset` / `scrollingElement.scrollTop` report the real position the engine is showing.
pub(crate) fn prim_scroll_y(scope: &mut Ctx, _args: NativeArgs, rv: &mut NativeReturn) {
    let y = host_state(scope).viewport_scroll_y.get();
    rv.set(Value::Num(y as f64));
}

/// `__prefersDark() -> boolean` — whether the effective OS appearance is Dark, read live from the
/// session host flag the engine sets (`set_color_scheme_dark`). Drives the JS `matchMedia`
/// `prefers-color-scheme` feature so it tracks the real macOS Light/Dark setting.
pub(crate) fn prim_prefers_dark(_scope: &mut Ctx, _args: NativeArgs, rv: &mut NativeReturn) {
    rv.set(Value::Bool(color_scheme_dark()));
}

/// `__scrollSet(y)` — request a scroll to document `y` (CSS px); clamped to >= 0.
pub(crate) fn prim_scroll_set(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let y = args.get(0).number_value(scope).unwrap_or(0.0);
    let y = if y.is_finite() { y.max(0.0) } else { 0.0 };
    set_pending_scroll(y.round() as f32);
}

pub(crate) fn prim_navigate(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let url = arg_str(scope, &args, 0);
    let replace = arg_str(scope, &args, 1) == "true";
    set_pending_navigation(url, replace);
}
pub(crate) fn prim_focused_node(_scope: &mut Ctx, _args: NativeArgs, rv: &mut NativeReturn) {
    rv.set_double(focused_node().map_or(-1.0, |id| id as f64));
}

/// `__scrollIntoView(id)` — request a scroll so node `id`'s top is near the viewport top.
pub(crate) fn prim_scroll_into_view(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let id = args.get(0).number_value(scope).unwrap_or(-1.0);
    if !id.is_finite() || id < 0.0 {
        return;
    }
    let target = host_state(scope)
        .layout_rects
        .borrow()
        .get(&(id as usize))
        .map(|&(_, y, _, _)| (y - 8.0).max(0.0)); // small margin above the element
    if let Some(t) = target {
        set_pending_scroll(t.round());
    }
}

/// Fill bytes using the platform CSPRNG; never substitute predictable randomness.
pub(crate) fn fill_random(buf: &mut [u8]) -> Result<(), getrandom::Error> {
    getrandom::fill(buf)
}

/// `__cryptoRandom(n) -> [byte, ...]` — `n` real random bytes (for `crypto.getRandomValues`/UUID).
pub(crate) fn prim_crypto_random(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let n = args.get(0).number_value(scope).unwrap_or(0.0);
    let n = if n.is_finite() && n > 0.0 {
        (n as usize).min(1 << 20)
    } else {
        0
    };
    let mut buf = vec![0u8; n];
    if fill_random(&mut buf).is_err() {
        // The JS boundary turns failure into OperationError without exposing any bytes.
        rv.set_null();
        return;
    }
    let arr = js_array(
        scope,
        buf.into_iter().map(|b| Value::Num(b as f64)).collect(),
    );
    rv.set(arr);
}

/// `__appendChild(parentId, childId)` — reparent child under parent.
pub(crate) fn prim_append_child(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let parent = arg_node(scope, &args, 0);
    let child = arg_node(scope, &args, 1);
    let state = host_state(scope);
    if let (Some(parent), Some(child)) = (parent, child) {
        state.bump_dom_version(); // invalidate getComputedStyle cache
        let old_parent = {
            let mut d = state.doc.borrow_mut();
            let old_parent = d.get(child).parent;
            if let Some(old_parent) = old_parent {
                d.get_mut(old_parent).children.retain(|&c| c != child);
            }
            d.get_mut(child).parent = Some(parent);
            d.get_mut(parent).children.push(child);
            old_parent
        };
        if state.observers_active.get() {
            // A move is a removal from the old parent + an addition to the new one.
            if let Some(old_parent) = old_parent {
                if old_parent != parent {
                    state.record_mutation(MutationRec {
                        kind: "childList",
                        target: old_parent,
                        attr_name: None,
                        old_value: None,
                        added: Vec::new(),
                        removed: vec![child],
                    });
                }
            }
            state.record_mutation(MutationRec {
                kind: "childList",
                target: parent,
                attr_name: None,
                old_value: None,
                added: vec![child],
                removed: Vec::new(),
            });
        }
    }
}

/// `__insertBefore(parentId, childId, refIdOrMinus1)`
pub(crate) fn prim_insert_before(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let parent = arg_node(scope, &args, 0);
    let child = arg_node(scope, &args, 1);
    // ref is -1 (append) or a node id.
    let ref_node = arg_node(scope, &args, 2);
    let state = host_state(scope);
    if let (Some(parent), Some(child)) = (parent, child) {
        state.bump_dom_version(); // invalidate getComputedStyle cache
        let old_parent = {
            let mut d = state.doc.borrow_mut();
            let old_parent = d.get(child).parent;
            if let Some(old) = old_parent {
                d.get_mut(old).children.retain(|&c| c != child);
            }
            d.get_mut(child).parent = Some(parent);
            let pos = ref_node.and_then(|r| d.get(parent).children.iter().position(|&c| c == r));
            match pos {
                Some(i) => d.get_mut(parent).children.insert(i, child),
                None => d.get_mut(parent).children.push(child),
            }
            old_parent
        };
        if state.observers_active.get() {
            if let Some(old) = old_parent {
                if old != parent {
                    state.record_mutation(MutationRec {
                        kind: "childList",
                        target: old,
                        attr_name: None,
                        old_value: None,
                        added: Vec::new(),
                        removed: vec![child],
                    });
                }
            }
            state.record_mutation(MutationRec {
                kind: "childList",
                target: parent,
                attr_name: None,
                old_value: None,
                added: vec![child],
                removed: Vec::new(),
            });
        }
    }
}

/// `__removeChild(parentId, childId)`
pub(crate) fn prim_remove_child(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let parent = arg_node(scope, &args, 0);
    let child = arg_node(scope, &args, 1);
    let state = host_state(scope);
    if let (Some(parent), Some(child)) = (parent, child) {
        state.bump_dom_version(); // invalidate getComputedStyle cache
        let removed = {
            let mut d = state.doc.borrow_mut();
            let was_child = d.get(parent).children.contains(&child);
            d.get_mut(parent).children.retain(|&c| c != child);
            if d.get(child).parent == Some(parent) {
                d.get_mut(child).parent = None;
            }
            was_child
        };
        if removed {
            state.record_mutation(MutationRec {
                kind: "childList",
                target: parent,
                attr_name: None,
                old_value: None,
                added: Vec::new(),
                removed: vec![child],
            });
        }
    }
}

/// `__children(id) -> [id...]` (all child nodes, in order)
pub(crate) fn prim_children(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let ids: Vec<dom::NodeId> = node
        .map(|n| state.doc.borrow().get(n).children.clone())
        .unwrap_or_default();
    let arr = js_id_array(scope, &ids);
    rv.set(arr);
}

/// `__parent(id) -> id | -1`
pub(crate) fn prim_parent(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let parent = node.and_then(|n| state.doc.borrow().get(n).parent);
    rv.set_double(parent.map(|p| p.0 as f64).unwrap_or(-1.0));
}

/// `__tag(id) -> string` (lowercased), or "" for non-elements.
pub(crate) fn prim_tag(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let tag = node
        .and_then(|n| match &state.doc.borrow().get(n).data {
            dom::NodeData::Element(e) => Some(e.tag.to_ascii_lowercase()),
            _ => None,
        })
        .unwrap_or_default();
    let s = js_str(scope, &tag);
    rv.set(s);
}

/// `__namespaceUri(id) -> string | null`
pub(crate) fn prim_namespace_uri(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let namespace = node.and_then(|n| match &state.doc.borrow().get(n).data {
        dom::NodeData::Element(element) => Some(
            element
                .namespace
                .as_deref()
                .unwrap_or("http://www.w3.org/1999/xhtml")
                .to_string(),
        ),
        _ => None,
    });
    match namespace {
        Some(namespace) => rv.set(js_str(scope, &namespace)),
        None => rv.set_null(),
    }
}

/// `__nodeType(id) -> 1 | 3 | 8 | 9`
pub(crate) fn prim_node_type(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let ty = node
        .map(|n| match &state.doc.borrow().get(n).data {
            dom::NodeData::Element(_) => 1,
            dom::NodeData::Text(_) => 3,
            dom::NodeData::Cdata(_) => 4,
            dom::NodeData::Comment(_) => 8,
            dom::NodeData::Document => 9,
            dom::NodeData::DocumentFragment => 11,
            dom::NodeData::ProcessingInstruction(_) => 7,
            dom::NodeData::DocumentType(_) => 10,
        })
        .unwrap_or(1);
    rv.set_int32(ty);
}

/// `__rect(id) -> { x, y, width, height, top, left, right, bottom } | null`
///
/// Looks `id` up in the engine-pushed `layout_rects`. The stored rect is document-absolute
/// (top-origin, CSS px); this returns it **viewport-relative** by subtracting `viewport_scroll_y`
/// vertically (there is no horizontal scroll, so `left == x_abs`). Returns `null` when the node has
/// no laid-out box (detached / display:none / before the first push), so the JS wrapper can fall
/// back to a zero-rect rather than throw.
pub(crate) fn prim_rect(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    // Reflect script DOM mutations the (blocked) engine hasn't rendered: re-lay-out in-Session if dirty.
    crate::forced_layout::ensure_layout_fresh(&state);
    let rect = node.and_then(|n| state.layout_rects.borrow().get(&n.0).copied());
    let (ax, ay, w, h) = match rect {
        Some(r) => r,
        None => {
            rv.set_null();
            return;
        }
    };
    let scroll_y = state.viewport_scroll_y.get();
    // Viewport-relative: subtract scroll (vertical only; no horizontal scroll tracked).
    let left = ax;
    let top = ay - scroll_y;
    let obj = Value::Obj(scope.new_object());
    let mut put = |k: &str, v: f32| {
        let key = k;
        let val = Value::Num(v as f64);
        let _ = scope.member_set(&obj, key, val);
    };
    put("x", left);
    put("y", top);
    put("left", left);
    put("top", top);
    put("right", left + w);
    put("bottom", top + h);
    put("width", w);
    put("height", h);
    rv.set(obj);
}

/// Minimal standard-base64 encoder (no deps): RGBA pixel blocks are bridged to JS as a base64
/// string which JS decodes with the built-in `atob`. Used by `__canvasPixels` for `getImageData`.
pub(crate) fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// `__canvasPixels(id, sx, sy, sw, sh) -> { w, h, b64 } | null`
///
/// Read a sub-rect of a `<canvas>`'s (or `<img>`'s) rasterized RGBA pixels, pushed back by the
/// engine after it rasterized the display list. Returns the clipped width/height plus a base64
/// string of `w*h*4` RGBA bytes (out-of-bounds pixels are transparent). `null` if no pixels exist
/// yet for that node (canvas not rendered — `getImageData` then returns a zeroed buffer). Reflects
/// the PREVIOUS frame's pixels (a one-render lag).
pub(crate) fn prim_canvas_pixels(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let id = args.get(0).number_value(scope).unwrap_or(-1.0);
    let sx = args.get(1).number_value(scope).unwrap_or(0.0) as i64;
    let sy = args.get(2).number_value(scope).unwrap_or(0.0) as i64;
    let sw = (args.get(3).number_value(scope).unwrap_or(0.0) as i64).max(0);
    let sh = (args.get(4).number_value(scope).unwrap_or(0.0) as i64).max(0);
    if id < 0.0 || sw == 0 || sh == 0 {
        rv.set_null();
        return;
    }
    let state = host_state(scope);
    let map = state.canvas_pixels.borrow();
    let (cw, ch, px) = match map.get(&(id as usize)) {
        Some(v) => v,
        None => {
            rv.set_null();
            return;
        }
    };
    let cw = *cw as i64;
    let ch = *ch as i64;
    // Copy the requested sub-rect, filling out-of-bounds with transparent black.
    let mut out = vec![0u8; (sw * sh * 4) as usize];
    for row in 0..sh {
        let srcy = sy + row;
        if srcy < 0 || srcy >= ch {
            continue;
        }
        for col in 0..sw {
            let srcx = sx + col;
            if srcx < 0 || srcx >= cw {
                continue;
            }
            let si = ((srcy * cw + srcx) * 4) as usize;
            let di = ((row * sw + col) * 4) as usize;
            out[di..di + 4].copy_from_slice(&px[si..si + 4]);
        }
    }
    let obj = Value::Obj(scope.new_object());
    let put_num = |scope: &mut Ctx, obj: &Value, k: &str, v: f64| {
        let key = k;
        let val = Value::Num(v);
        let _ = scope.member_set(obj, key, val);
    };
    put_num(scope, &obj, "w", sw as f64);
    put_num(scope, &obj, "h", sh as f64);
    let b64 = base64_encode(&out);
    let key = "b64";
    let val = js_str(scope, &b64);
    let _ = scope.member_set(&obj, key, val);
    rv.set(obj);
}

/// `__rasterizeCanvas(commandsJson, width, height) -> string | null`
///
/// Synchronously rasterize a SINGLE canvas's 2D display list into straight-alpha RGBA8 pixels
/// (`width*height*4` bytes) and return them base64-encoded (JS decodes with `atob`). Returns `null`
/// if the JSON cannot be parsed into a canvas list. This powers `OffscreenCanvas` operations that
/// must read pixels back in-Session (getImageData / convertToBlob / transferToImageBitmap) without a
/// round-trip through the engine.
///
/// `commandsJson` is the SAME shape `__canvasLists()` produces for one canvas — a one-element JSON
/// array `[{id,width,height,commands:[...]}]` — so the JS side can reuse its existing serializer.
/// `width`/`height` are passed for the caller's convenience but the bitmap size is taken from the
/// parsed canvas list entry (its `width`/`height`), matching `parse_canvas_lists`.
///
/// NOTE: text glyphs and `drawImage`-from-node are NOT rendered here yet — there is no system font
/// or decoded-source map available in-Session, so `font = None` and an empty `sources` map are used.
/// Those ops are simply skipped (acceptable for offscreen for now).
pub(crate) fn prim_rasterize_canvas(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let json = arg_str(scope, &args, 0);
    let _width = args.get(1).number_value(scope).unwrap_or(0.0);
    let _height = args.get(2).number_value(scope).unwrap_or(0.0);
    let lists = paint::canvas::parse_canvas_lists(&json);
    let cv = match lists.first() {
        Some(cv) => cv,
        None => {
            rv.set_null();
            return;
        }
    };
    // No system font / drawImage sources in-Session: text + drawImage-from-node are skipped.
    let sources: std::collections::HashMap<usize, (&[u8], u32, u32)> =
        std::collections::HashMap::new();
    let img = paint::canvas::rasterize_canvas(cv, None, &sources);
    let b64 = base64_encode(&img.rgba);
    let val = js_str(scope, &b64);
    rv.set(val);
}

/// `__naturalSize(id) -> { w, h }`
///
/// The decoded intrinsic size of an `<img>` (CSS px), pushed by the engine alongside the layout
/// rects from its decoded-bitmap table. Backs `img.naturalWidth` / `img.naturalHeight`. A
/// missing/broken/not-yet-decoded image has no entry and reports `{ w: 0, h: 0 }`.
pub(crate) fn prim_natural_size(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let (w, h) = node
        .and_then(|n| state.image_natural.borrow().get(&n.0).copied())
        .unwrap_or((0.0, 0.0));
    let obj = Value::Obj(scope.new_object());
    let mut put = |k: &str, v: f32| {
        let key = k;
        let val = Value::Num(v as f64);
        let _ = scope.member_set(&obj, key, val);
    };
    put("w", w);
    put("h", h);
    rv.set(obj);
}

/// `__detachArrayBuffer(value) -> bool` — detach a Lumen ArrayBuffer after structured-clone copied it.
pub(crate) fn prim_detach_array_buffer(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let global = scope.global_this();
    let byte_length = scope
        .realm_value_for_host(&global, "browser.arrayBufferByteLength")
        .expect("ArrayBuffer getter");
    if scope.invoke(byte_length, args.get(0), &[]).is_err() {
        rv.set_bool(false);
        return;
    }
    let detach = scope
        .realm_value_for_host(&global, "browser.detach")
        .expect("detach intrinsic");
    let detached = scope
        .invoke(detach, Value::Undefined, &[args.get(0)])
        .is_ok();
    rv.set(Value::Bool(detached));
}

/// `__elemMetrics(id) -> { ow, oh, ot, ol, sw, sh } | null`
///
/// Box metrics for `offsetWidth/Height/Top/Left`, `clientWidth/Height`, `scrollWidth/Height`:
/// - `ow`/`oh` = border-box width/height (offsetWidth/Height; clientWidth/Height ≈ same — we do
///   not subtract borders/scrollbars).
/// - `ot`/`ol` = document-absolute top/left (offsetTop/Left — a simplification; real offsetTop is
///   relative to `offsetParent`, but we report absolute coordinates).
/// - `sw`/`sh` = the union of the element's client box and laid-out descendant border boxes.
///   The document root / `<html>` / `<body>` use the full document height.
/// Returns `null` when the node has no laid-out box.
pub(crate) fn prim_elem_metrics(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    crate::forced_layout::ensure_layout_fresh(&state);
    let rect = node.and_then(|n| state.layout_rects.borrow().get(&n.0).copied());
    // Client box (padding box: content + padding, excluding borders) computed from the cascade.
    // `clientWidth`/`clientHeight` exclude borders/scrollbars, so they cannot reuse the border-box
    // `ow`/`oh`.
    let client_box = node.and_then(|n| {
        rect.map(|(_, _, w, h)| {
            with_cascade_map(&state, |_doc, map| {
                map.get(&n).map_or((w, h), |cs| {
                    let laid_out = (
                        (w - cs.border.left - cs.border.right).max(0.0),
                        (h - cs.border.top - cs.border.bottom).max(0.0),
                    );
                    let specified = (
                        cs.width.unwrap_or(0.0) + cs.padding.left + cs.padding.right,
                        cs.height.unwrap_or(0.0) + cs.padding.top + cs.padding.bottom,
                    );
                    (
                        if cs.contain_width {
                            laid_out.0
                        } else {
                            specified.0
                        },
                        if cs.contain_height {
                            laid_out.1
                        } else {
                            specified.1
                        },
                    )
                })
            })
        })
    });
    let (ax, ay, w, h) = match rect {
        Some(r) => r,
        None => {
            // No laid-out box yet (the engine hasn't re-laid-out since the style change). Synthesize
            // a border box from the cascade so reads like `clientHeight` reflect explicit sizes.
            match node.and_then(|n| {
                with_cascade_map(&state, |_doc, map| {
                    map.get(&n).map(|cs| {
                        let bw = cs.width.unwrap_or(0.0)
                            + cs.padding.left
                            + cs.padding.right
                            + cs.border.left
                            + cs.border.right;
                        let bh = cs.height.unwrap_or(0.0)
                            + cs.padding.top
                            + cs.padding.bottom
                            + cs.border.top
                            + cs.border.bottom;
                        (bw, bh)
                    })
                })
            }) {
                Some((bw, bh)) if bw > 0.0 || bh > 0.0 => (0.0, 0.0, bw, bh),
                _ => {
                    rv.set_null();
                    return;
                }
            }
        }
    };
    // Document-root special case: report the full page height as scrollHeight so sites that size
    // off `documentElement.scrollHeight` / `body.scrollHeight` see the real content height.
    let is_root = node
        .map(|n| {
            let doc = state.doc.borrow();
            match &doc.get(n).data {
                dom::NodeData::Document => true,
                dom::NodeData::Element(e) => {
                    e.tag.eq_ignore_ascii_case("html") || e.tag.eq_ignore_ascii_case("body")
                }
                _ => false,
            }
        })
        .unwrap_or(false);
    let (sw, sh) = if is_root {
        // Viewport width ≈ the root border-box width here; full document height for scrollHeight.
        (w, state.doc_height.get().max(h))
    } else {
        // CSSOM View's scrolling area includes positive overflow from descendants. Walk the DOM
        // ancestry for every laid-out rect so nested boxes contribute even though the layout table
        // is flat by node id here. Negative overflow does not extend the start edge.
        let rects = state.layout_rects.borrow();
        let doc = state.doc.borrow();
        let mut right = ax + client_box.map(|v| v.0).unwrap_or(w);
        let mut bottom = ay + client_box.map(|v| v.1).unwrap_or(h);
        if let Some(owner) = node {
            for (&id, &(rx, ry, rw, rh)) in rects.iter() {
                let mut cur = if id < doc.len() {
                    doc.get(dom::NodeId(id)).parent
                } else {
                    None
                };
                let mut descendant = false;
                while let Some(parent) = cur {
                    if parent == owner {
                        descendant = true;
                        break;
                    }
                    cur = doc.get(parent).parent;
                }
                if descendant {
                    right = right.max(rx + rw);
                    bottom = bottom.max(ry + rh);
                }
            }
        }
        ((right - ax).max(w), (bottom - ay).max(h))
    };
    // `offsetTop`/`offsetLeft` are relative to the offsetParent — the nearest positioned ancestor
    // (per CSSOM-View): subtract its border-box origin and its top/left border widths
    // (clientTop/clientLeft). With no positioned ancestor we keep document-absolute coordinates
    // (offsetParent ≈ body at the origin), preserving prior behavior. `ax`/`ay` are the node's
    // document-absolute border-box origin (CSS px), the same space `layout_rects` stores.
    let (ol, ot) = {
        let rects = state.layout_rects.borrow();
        with_cascade_map(&state, |doc, map| {
            let mut cur = node.and_then(|n| doc.get(n).parent);
            let mut positioned = None;
            while let Some(p) = cur {
                if matches!(doc.get(p).data, dom::NodeData::Element(_))
                    && map
                        .get(&p)
                        .map(|cs| cs.position != style::Position::Static)
                        .unwrap_or(false)
                {
                    positioned = Some(p);
                    break;
                }
                cur = doc.get(p).parent;
            }
            match positioned.and_then(|p| rects.get(&p.0).map(|r| (p, *r))) {
                Some((p, (px, py, _, _))) => {
                    let (bl, bt) = map
                        .get(&p)
                        .map(|cs| (cs.border.left, cs.border.top))
                        .unwrap_or((0.0, 0.0));
                    (ax - px - bl, ay - py - bt)
                }
                None => (ax, ay),
            }
        })
    };
    let obj = Value::Obj(scope.new_object());
    let mut put = |k: &str, v: f32| {
        let key = k;
        let val = Value::Num(v as f64);
        let _ = scope.member_set(&obj, key, val);
    };
    // For the document root (`<html>`/`<body>`/Document), `clientWidth`/`clientHeight` are the
    // viewport's content box — approximated by the laid-out border box (`w`/`h`) — rather than the
    // cascade `width`/`height` (which are `auto` → 0 for an unsized root). Without this,
    // `documentElement.clientWidth` would read 0 and break viewport-bounds math (CSSOM-View tests).
    let (cw, ch) = if is_root {
        (w, h)
    } else {
        client_box.unwrap_or((w, h))
    };
    put("ow", w);
    put("oh", h);
    put("cw", cw);
    put("ch", ch);
    put("ot", ot);
    put("ol", ol);
    put("sw", sw);
    put("sh", sh);
    rv.set(obj);
}

/// `__textContent(id) -> string`
pub(crate) fn prim_text_content(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let s = node
        .map(|n| text_content(&state.doc.borrow(), n))
        .unwrap_or_default();
    let v = js_str(scope, &s);
    rv.set(v);
}

/// `__setTextContent(id, text)`
pub(crate) fn prim_set_text_content(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let text = arg_str(scope, &args, 1);
    let state = host_state(scope);
    if let Some(n) = node {
        state.bump_dom_version(); // invalidate getComputedStyle cache
                                  // For Text/Comment nodes, setting textContent/data is a `characterData` mutation; capture
                                  // the old string value first (for `characterDataOldValue`). For elements it replaces the
                                  // subtree (we don't emit a childList record for that simplification).
        let char_old = if state.observers_active.get() {
            match &state.doc.borrow().get(n).data {
                dom::NodeData::Text(t) => Some(("characterData", t.clone())),
                dom::NodeData::Comment(c) => Some(("characterData", c.clone())),
                _ => None,
            }
        } else {
            None
        };
        set_text_content(&mut state.doc.borrow_mut(), n, &text);
        if let Some((_, old)) = char_old {
            state.record_mutation(MutationRec {
                kind: "characterData",
                target: n,
                attr_name: None,
                old_value: Some(old),
                added: Vec::new(),
                removed: Vec::new(),
            });
        }
    }
}

/// `__innerHTML(id) -> string`
pub(crate) fn prim_inner_html(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let s = node
        .map(|n| inner_html(&state.doc.borrow(), n))
        .unwrap_or_default();
    let v = js_str(scope, &s);
    rv.set(v);
}

/// `__outerHTML(id) -> string`
pub(crate) fn prim_outer_html(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let s = node
        .map(|n| outer_html(&state.doc.borrow(), n))
        .unwrap_or_default();
    let v = js_str(scope, &s);
    rv.set(v);
}

/// `__setInnerHTML(id, html)`
pub(crate) fn prim_set_inner_html(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let html = arg_str(scope, &args, 1);
    let state = host_state(scope);
    if let Some(n) = node {
        state.bump_dom_version(); // invalidate getComputedStyle cache (subtree replaced)
        set_inner_html(&mut state.doc.borrow_mut(), n, &html);
    }
}

/// `__parseHtmlSections(headId, bodyId, html)` — parse a full HTML document string and copy its
/// parsed <head>/<body> children under the given (fresh, detached) head/body nodes. Backs
/// DOMParser.parseFromString(…, "text/html"), which must return an independent document rather than
/// the live one.
pub(crate) fn prim_parse_html_sections(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let head = arg_node(scope, &args, 0);
    let body = arg_node(scope, &args, 1);
    let html = arg_str(scope, &args, 2);
    let state = host_state(scope);
    state.bump_dom_version();
    parse_html_into_sections(&mut state.doc.borrow_mut(), head, body, &html);
}

/// `__innerText(id) -> string` — the `innerText`/`outerText` getter (rendered-text algorithm).
pub(crate) fn prim_inner_text(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    // innerText/outerText are HTMLElement-only; leave the return value `undefined` for SVG/MathML.
    let s = node.and_then(|n| {
        with_cascade_map(&state, |doc, map| {
            inner_text::is_html_element(doc, n).then(|| inner_text::inner_text(doc, map, n))
        })
    });
    if let Some(s) = s {
        let v = js_str(scope, &s);
        rv.set(v);
    }
}

/// `__setInnerText(id, text)` — the `innerText` setter (replace children with a rendered fragment).
pub(crate) fn prim_set_inner_text(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let text = arg_str(scope, &args, 1);
    let state = host_state(scope);
    if let Some(n) = node {
        // No-op on SVG/MathML elements (innerText is HTMLElement-only).
        if !inner_text::is_html_element(&state.doc.borrow(), n) {
            return;
        }
        state.bump_dom_version(); // invalidate getComputedStyle cache (subtree replaced)
        inner_text::set_inner_text(&mut state.doc.borrow_mut(), n, &text);
    }
}

/// `__setOuterText(id, text) -> bool` — the `outerText` setter. Returns `false` when the element has
/// no parent (the JS wrapper then throws `NoModificationAllowedError`).
pub(crate) fn prim_set_outer_text(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node = arg_node(scope, &args, 0);
    let text = arg_str(scope, &args, 1);
    let state = host_state(scope);
    let ok = match node {
        // No-op (but no error) on SVG/MathML elements (outerText is HTMLElement-only).
        Some(n) if !inner_text::is_html_element(&state.doc.borrow(), n) => true,
        Some(n) => {
            state.bump_dom_version();
            inner_text::set_outer_text(&mut state.doc.borrow_mut(), n, &text)
        }
        None => false,
    };
    rv.set_bool(ok);
}

/// `__getElementById(idStr) -> id | -1`
pub(crate) fn prim_get_element_by_id(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let id = arg_str(scope, &args, 0);
    let state = host_state(scope);
    let found = {
        let d = state.doc.borrow();
        find_by_id(&d, d.root(), &id)
    };
    rv.set_double(found.map(|n| n.0 as f64).unwrap_or(-1.0));
}

/// `__querySelectorAll(sel) -> [id...]`
pub(crate) fn prim_query_selector_all(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let sel = arg_str(scope, &args, 0);
    let state = host_state(scope);
    let ids = {
        let d = state.doc.borrow();
        query_selector_all(&d, &sel)
    };
    let arr = js_id_array(scope, &ids);
    rv.set(arr);
}

/// `__querySelectorAllWithin(rootId, sel) -> [id...]`
pub(crate) fn prim_query_selector_all_within(
    scope: &mut Ctx,
    args: NativeArgs,
    rv: &mut NativeReturn,
) {
    let root = arg_node(scope, &args, 0);
    let sel = arg_str(scope, &args, 1);
    let state = host_state(scope);
    let ids = match root {
        Some(root) => {
            let d = state.doc.borrow();
            query_within(&d, root, &sel)
        }
        None => Vec::new(),
    };
    let arr = js_id_array(scope, &ids);
    rv.set(arr);
}

/// `__getElementsByTagName(tag) -> [id...]` (whole document)
pub(crate) fn prim_get_elements_by_tag_name(
    scope: &mut Ctx,
    args: NativeArgs,
    rv: &mut NativeReturn,
) {
    let tag = arg_str(scope, &args, 0);
    let state = host_state(scope);
    let mut ids = Vec::new();
    {
        let d = state.doc.borrow();
        collect_by_tag(&d, d.root(), &tag, &mut ids);
    }
    let arr = js_id_array(scope, &ids);
    rv.set(arr);
}

/// `__getElementsByTagNameWithin(rootId, tag) -> [id...]` (excludes root itself)
pub(crate) fn prim_get_elements_by_tag_name_within(
    scope: &mut Ctx,
    args: NativeArgs,
    rv: &mut NativeReturn,
) {
    let root = arg_node(scope, &args, 0);
    let tag = arg_str(scope, &args, 1);
    let state = host_state(scope);
    let mut ids = Vec::new();
    if let Some(root) = root {
        let d = state.doc.borrow();
        for &child in &d.get(root).children {
            collect_by_tag(&d, child, &tag, &mut ids);
        }
    }
    let arr = js_id_array(scope, &ids);
    rv.set(arr);
}

/// `__getElementsByClassName(cls) -> [id...]` (space-separated = all required)
pub(crate) fn prim_get_elements_by_class_name(
    scope: &mut Ctx,
    args: NativeArgs,
    rv: &mut NativeReturn,
) {
    let raw = arg_str(scope, &args, 0);
    let wanted: Vec<String> = raw.split_whitespace().map(|s| s.to_string()).collect();
    let state = host_state(scope);
    let mut ids = Vec::new();
    {
        let d = state.doc.borrow();
        collect_by_class(&d, d.root(), &wanted, &mut ids);
    }
    let arr = js_id_array(scope, &ids);
    rv.set(arr);
}

/// `__documentElementId() -> id | -1` (the root element: `<html>` for HTML, `<svg>`/… for XML)
pub(crate) fn prim_document_element_id(scope: &mut Ctx, _args: NativeArgs, rv: &mut NativeReturn) {
    let state = host_state(scope);
    let found = {
        let d = state.doc.borrow();
        // The document element is the root element child of the Document node — `<html>` for an HTML
        // document, but `<svg>`/`<...>` for an XML document. Return the first Element child of root.
        d.get(d.root())
            .children
            .iter()
            .copied()
            .find(|&c| matches!(d.get(c).data, dom::NodeData::Element(_)))
    };
    rv.set_double(found.map(|n| n.0 as f64).unwrap_or(-1.0));
}

/// `__documentRootId() -> id` — the Document node itself (arena root). Always valid, even when the
/// root element has been replaced/removed, so JS can find the document's children directly.
pub(crate) fn prim_document_root_id(scope: &mut Ctx, _args: NativeArgs, rv: &mut NativeReturn) {
    let root = host_state(scope).doc.borrow().root();
    rv.set_double(root.0 as f64);
}

/// `__bodyId() -> id | -1`
pub(crate) fn prim_body_id(scope: &mut Ctx, _args: NativeArgs, rv: &mut NativeReturn) {
    let state = host_state(scope);
    let found = {
        let d = state.doc.borrow();
        find_by_tag(&d, d.root(), "body")
    };
    rv.set_double(found.map(|n| n.0 as f64).unwrap_or(-1.0));
}

/// `__headId() -> id | -1`
pub(crate) fn prim_head_id(scope: &mut Ctx, _args: NativeArgs, rv: &mut NativeReturn) {
    let state = host_state(scope);
    let found = {
        let d = state.doc.borrow();
        find_by_tag(&d, d.root(), "head")
    };
    rv.set_double(found.map(|n| n.0 as f64).unwrap_or(-1.0));
}

/// `__rootId() -> id` (the Document root node)
pub(crate) fn prim_root_id(scope: &mut Ctx, _args: NativeArgs, rv: &mut NativeReturn) {
    let state = host_state(scope);
    let root = state.doc.borrow().root();
    rv.set_double(root.0 as f64);
}

/// `__titleText() -> string`
pub(crate) fn prim_title_text(scope: &mut Ctx, _args: NativeArgs, rv: &mut NativeReturn) {
    let state = host_state(scope);
    let s = {
        let d = state.doc.borrow();
        find_by_tag(&d, d.root(), "title")
            .map(|n| text_content(&d, n))
            .unwrap_or_default()
    };
    let v = js_str(scope, &s);
    rv.set(v);
}

/// Append `"key":"<escaped value>"` to a JSON object body, with a trailing comma.
fn json_field(out: &mut String, key: &str, value: &str) {
    out.push('"');
    out.push_str(key);
    out.push_str("\":");
    json_escape_into(value, out);
    out.push(',');
}

/// Append a JSON string literal (with surrounding quotes) for `s` to `out`.
fn json_escape_into(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Serialize a parsed URL into the component record the JS `URL`/`parseURL` layer expects.
fn url_to_record(u: &wurl::Url) -> String {
    let mut s = String::from("{");
    json_field(&mut s, "href", &u.href());
    json_field(&mut s, "protocol", &format!("{}:", u.scheme()));
    json_field(&mut s, "username", u.username());
    json_field(&mut s, "password", u.password());
    json_field(&mut s, "host", &u.host_str());
    json_field(&mut s, "hostname", &u.hostname());
    json_field(&mut s, "port", &u.port_str());
    json_field(&mut s, "pathname", &u.path_str());
    json_field(&mut s, "search", &u.query_str());
    json_field(&mut s, "hash", &u.fragment_str());
    json_field(&mut s, "origin", &u.origin());
    s.push_str(&format!("\"opaque\":{}", u.cannot_be_a_base()));
    s.push('}');
    s
}

/// `__urlParse(input, base|null) -> recordJSON | null`. WHATWG URL parsing via our spec-compliant
/// parser ([`wurl`]). Returns the component record as JSON, or null on a parse failure
/// (the JS `URL` constructor then throws).
pub(crate) fn prim_url_parse(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    use wurl::Url;
    let input = arg_str(scope, &args, 0);
    let base_arg = args.get(1);
    // Optional 3rd arg: the document's character encoding (for query encoding in non-UTF-8 docs).
    let enc = args.get(2);
    let enc = if enc.is_string() {
        Some(enc.to_rust_string_lossy(scope))
    } else {
        None
    };
    let base_str = if base_arg.is_string() {
        Some(base_arg.to_rust_string_lossy(scope))
    } else {
        None
    };
    let base = base_str.as_deref().and_then(|b| Url::parse(b).ok());
    let parsed = match (&base_str, &base) {
        // A base was given but didn't parse -> failure.
        (Some(_), None) => Err(()),
        _ => match &enc {
            Some(e) => Url::parse_in_document(&input, base.as_ref(), e),
            None => match base.as_ref() {
                Some(b) => Url::parse_with_base(&input, b),
                None => Url::parse(&input),
            },
        },
    };
    match parsed {
        Ok(u) => {
            let rec = url_to_record(&u);
            let v = js_str(scope, &rec);
            rv.set(v);
        }
        Err(_) => rv.set_null(),
    }
}

/// `__urlSet(href, prop, value) -> recordJSON | null`. Apply a WHATWG URL setter (protocol/username/
/// password/host/hostname/port/pathname/search/hash) to an already-valid `href` and reserialize.
/// Returns the updated record, or null if `href` itself doesn't parse. Invalid setter values are
/// ignored (no-op) exactly as the URL setters specify.
pub(crate) fn prim_url_set(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    use wurl::Url;
    let href = arg_str(scope, &args, 0);
    let prop = arg_str(scope, &args, 1);
    // Pass the raw value: the parser-based setters strip ASCII tab/newline inside the basic parser
    // (so a value like "\n\n\t" for `port` is non-empty here, runs through the parser to an empty
    // buffer, and leaves the port unchanged — rather than being treated as the empty string), and
    // username/password percent-encode tab/newline (%09/%0A/%0D).
    let value = arg_str(scope, &args, 2);
    let mut u = match Url::parse(&href) {
        Ok(u) => u,
        Err(_) => {
            rv.set_null();
            return;
        }
    };
    // The href setter signals an invalid value (which the JS layer turns into a throw) by failing.
    if prop == "href" && Url::parse(&value).is_err() {
        rv.set_null();
        return;
    }
    u.set(&prop, &value);
    let rec = url_to_record(&u);
    let v = js_str(scope, &rec);
    rv.set(v);
}

/// `__formDecode(s) -> string`. application/x-www-form-urlencoded decode: `+` -> space, percent-decode
/// each valid `%XX`, then UTF-8 decode with replacement (invalid byte sequences -> U+FFFD). Done in
/// Rust so invalid UTF-8 (e.g. `%FE%FF`) yields replacement characters, matching the URL standard.
pub(crate) fn prim_form_decode(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let s = arg_str(scope, &args, 0);
    let b = s.as_bytes();
    let mut bytes: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'+' {
            bytes.push(b' ');
            i += 1;
        } else if b[i] == b'%'
            && i + 2 < b.len()
            && b[i + 1].is_ascii_hexdigit()
            && b[i + 2].is_ascii_hexdigit()
        {
            bytes.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap_or(0));
            i += 3;
        } else {
            bytes.push(b[i]);
            i += 1;
        }
    }
    let decoded = String::from_utf8_lossy(&bytes);
    let v = js_str(scope, &decoded);
    rv.set(v);
}

/// `__fetch(url) -> string | null`
///
/// Synchronous network primitive backing JS `fetch()`. Resolves `url` against `globalThis.__pageURL`
/// (absolute URLs pass through unchanged; relative ones are joined onto the page URL), calls the host
/// fetcher, and returns the response body as a string. Returns `null` (JS) on any failure — bad URL,
/// no fetcher result, etc. Runs on the isolate's own worker thread, so the blocking host fetch is
/// fine. Never panics.
pub(crate) fn prim_fetch(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let raw = arg_str(scope, &args, 0);
    // Resolve against the page URL when present (so relative URLs work like other fetches).
    let resolved = {
        let global = scope.global_this();
        let key = "__pageURL";
        let base = scope
            .member_get(&global, key)
            .ok()
            .filter(|v| v.is_string())
            .map(|v| v.to_rust_string_lossy(scope));
        match base {
            Some(b) if !b.is_empty() => wurl::resolve(&raw, &b).unwrap_or_else(|| raw.clone()),
            _ => raw.clone(),
        }
    };
    let body = (host_state(scope).fetcher)(&resolved).map(|(b, _)| b);
    match body {
        Some(s) => {
            let v = js_str(scope, &s);
            rv.set(v);
        }
        None => rv.set_null(),
    }
}

/// `__request(method, url, body, headersJson) -> string | null`
///
/// Arbitrary-method network primitive backing the rewritten JS `fetch()`. Resolves `url` against
/// `globalThis.__pageURL` (relative URLs join onto the page URL; absolute ones pass through), then
/// calls the host `request_fetcher` with the (method, resolved-url, body, headers-JSON) and returns
/// the response *envelope* JSON string the host produced. Returns `null` (JS) on transport failure
/// or when no request fetcher is installed. Runs on the isolate's own worker thread, so the
/// blocking host request is fine. Never panics.
pub(crate) fn prim_request(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let method = arg_str(scope, &args, 0);
    let raw = arg_str(scope, &args, 1);
    let body = arg_str(scope, &args, 2);
    let headers_json = arg_str(scope, &args, 3);
    // Resolve against the page URL when present (so relative URLs work like other fetches).
    let resolved = {
        let global = scope.global_this();
        let key = "__pageURL";
        let base = scope
            .member_get(&global, key)
            .ok()
            .filter(|v| v.is_string())
            .map(|v| v.to_rust_string_lossy(scope));
        match base {
            Some(b) if !b.is_empty() => wurl::resolve(&raw, &b).unwrap_or_else(|| raw.clone()),
            _ => raw.clone(),
        }
    };
    let envelope = (host_state(scope).request_fetcher)(&method, &resolved, &body, &headers_json);
    match envelope {
        Some(s) => {
            let v = js_str(scope, &s);
            rv.set(v);
        }
        None => rv.set_null(),
    }
}

/// `__startFetch(method, url, body, headersJson) -> id (number)`
///
/// Non-blocking sibling of `__request` backing the async JS `fetch()`. Resolves `url` against
/// `__pageURL` (on the worker thread, like `__request`), allocates a request id, then spawns a
/// **background thread** that runs the (`Send + Sync`) host `request_fetcher` and `send`s
/// `(id, envelope-or-None)` back over the worker's completion channel. Returns the id to JS
/// immediately so `fetch()` can store its promise resolvers under it. The background thread NEVER
/// touches Lumen; the promise is settled later on the worker thread inside [`drain_event_loop`] via
/// `__resolveFetch`/`__rejectFetch`. Increments the in-flight counter so the drain keeps looping
/// until this completion is pulled.
pub(crate) fn prim_start_fetch(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let method = arg_str(scope, &args, 0);
    let raw = arg_str(scope, &args, 1);
    let body = arg_str(scope, &args, 2);
    let headers_json = arg_str(scope, &args, 3);
    // Resolve against the page URL when present (so relative URLs work like other fetches).
    let resolved = {
        let global = scope.global_this();
        let key = "__pageURL";
        let base = scope
            .member_get(&global, key)
            .ok()
            .filter(|v| v.is_string())
            .map(|v| v.to_rust_string_lossy(scope));
        match base {
            Some(b) if !b.is_empty() => wurl::resolve(&raw, &b).unwrap_or_else(|| raw.clone()),
            _ => raw.clone(),
        }
    };

    let state = host_state(scope);
    let id = state.next_fetch_id.fetch_add(1, Ordering::Relaxed);
    let request_fetcher = Arc::clone(&state.request_fetcher);
    let tx = state.fetch_tx.clone();
    state.in_flight.set(state.in_flight.get() + 1);

    // Thread-per-request: sites fire a bounded handful concurrently, so this is fine. The work is
    // pure host I/O; it never re-enters Lumen. On spawn failure we synchronously deliver `None` so the
    // promise still rejects and the in-flight count is reconciled by the drain.
    let spawned = std::thread::Builder::new()
        .name("js-fetch".to_string())
        .spawn(move || {
            let env = request_fetcher(&method, &resolved, &body, &headers_json);
            let _ = tx.send((id, env));
        });
    if spawned.is_err() {
        let _ = state.fetch_tx.send((id, None));
    }

    rv.set(Value::Num(id as f64));
}

/// `__wsConnect(url) -> id (number)`
///
/// Backs `new WebSocket(url)`. Allocates a socket id, then asks the host `ws_connector` to spawn a
/// background socket thread (which runs `net::ws_run`). On success the per-socket outgoing sender is
/// stored under the id so `__wsSend`/`__wsClose` can reach the socket; on failure we synthesize an
/// `error` (kind 4) + `close` (kind 3) event so the JS object still fires onerror/onclose. The id is
/// always returned so the JS object can register itself in `__wsRegistry` either way.
pub(crate) fn prim_ws_connect(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let raw = arg_str(scope, &args, 0);
    // Resolve against the page URL when present (so a relative ws path works), like fetch.
    let resolved = {
        let global = scope.global_this();
        let key = "__pageURL";
        let base = scope
            .member_get(&global, key)
            .ok()
            .filter(|v| v.is_string())
            .map(|v| v.to_rust_string_lossy(scope));
        match base {
            Some(b) if !b.is_empty() => wurl::resolve(&raw, &b).unwrap_or_else(|| raw.clone()),
            _ => raw.clone(),
        }
    };

    let state = host_state(scope);
    let id = state.next_ws_id.fetch_add(1, Ordering::Relaxed);
    let evt_tx = state.ws_evt_tx.clone();
    match (state.ws_connector)(resolved, id, evt_tx) {
        Ok(out_tx) => {
            state.ws_senders.borrow_mut().insert(id, out_tx);
        }
        Err(msg) => {
            // No socket thread: synthesize the error + close so onerror/onclose still fire.
            let _ = state.ws_evt_tx.send((id, 4, msg));
            let _ = state.ws_evt_tx.send((id, 3, "1006:".to_string()));
        }
    }
    rv.set(Value::Num(id as f64));
}

/// `__wsSend(id, kind, payload)` — enqueue an outgoing frame on socket `id`. kind 0 = text,
/// 1 = binary (payload is base64). No-op if the id is unknown (already closed).
pub(crate) fn prim_ws_send(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let id = arg_str(scope, &args, 0).parse::<u64>().unwrap_or(0);
    let kind = arg_str(scope, &args, 1).parse::<u8>().unwrap_or(0);
    let payload = arg_str(scope, &args, 2);
    let state = host_state(scope);
    // Clone the sender out (cheap; `Sender` is `Clone`) so the `RefCell` borrow ends before send.
    let tx = state.ws_senders.borrow().get(&id).cloned();
    if let Some(tx) = tx {
        let _ = tx.send((kind, payload));
    }
}

/// `__wsClose(id)` — ask socket `id` to close and forget its sender (the socket thread exits when
/// its receiver drops / it observes the close command). No-op if the id is unknown.
pub(crate) fn prim_ws_close(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let id = arg_str(scope, &args, 0).parse::<u64>().unwrap_or(0);
    let state = host_state(scope);
    let tx = state.ws_senders.borrow_mut().remove(&id);
    if let Some(tx) = tx {
        let _ = tx.send((2, String::new()));
    }
}

// ---------------------------------------------------------------------------------------------
// Installation: register native primitives + evaluate the JS bootstrap onto a fresh context.
// ---------------------------------------------------------------------------------------------

/// Define a native function on `target` under `name`.
pub(crate) fn set_fn(
    scope: &mut Ctx,
    target: &Value,
    name: &str,
    cb: fn(&mut Ctx, NativeArgs, &mut NativeReturn),
) {
    let func = scope.new_native_fn(
        name,
        0,
        Rc::new(move |scope, _this, args| {
            let mut rv = NativeReturn::default();
            cb(scope, NativeArgs::new(args), &mut rv);
            rv.finish()
        }),
    );
    scope
        .member_set(target, name, func)
        .unwrap_or_else(|_| panic!("native function installation"));
}

/// Install the `__consoleLog` native sink. The JS `console` object (built in the bootstrap) calls
/// it. On the no-DOM `Runtime::eval` path it is the only thing installed besides timers.
pub(crate) fn install_console_sink(scope: &mut Ctx, global: &Value) {
    set_fn(scope, global, "__consoleLog", prim_console_log);
    // A minimal `console` whose methods all funnel into `__consoleLog`. (The browser-env bootstrap
    // does not touch console; this is the canonical one used everywhere.)
    let src = r#"
    (function () {
      function log() { __consoleLog.apply(null, Array.prototype.slice.call(arguments)); }
      globalThis.console = { log: log, info: log, warn: log, error: log, debug: log,
        trace: log, dir: log, table: log, group: log, groupEnd: function(){}, groupCollapsed: log,
        assert: function(c){ if(!c){ log.apply(null, Array.prototype.slice.call(arguments,1)); } },
        count: function(){}, time: function(){}, timeEnd: function(){} };
    })();
    "#;
    eval_internal(scope, src, "<console>");
}

/// `__observersActive(bool)` — JS sets this true when the first `MutationObserver` is registered
/// and false when the last disconnects. Gates whether the mutation primitives record anything.
/// `__commitSelection(startEl, startOff, endEl, endOff)` — record the active programmatic text
/// selection (boundary *elements* + char offsets, document order) so the engine can paint the
/// `::selection` highlight. Called by `getSelection()` mutators. Any missing/invalid boundary clears
/// the selection.
pub(crate) fn prim_commit_selection(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let sel = match (arg_node(scope, &args, 0), arg_node(scope, &args, 2)) {
        (Some(s), Some(e)) => {
            let so = args.get(1).number_value(scope).unwrap_or(0.0).max(0.0) as u32;
            let eo = args.get(3).number_value(scope).unwrap_or(0.0).max(0.0) as u32;
            Some((s.0, so, e.0, eo))
        }
        _ => None,
    };
    crate::set_active_selection(sel);
}

/// `__clearHighlights()` — drop all CSS Custom Highlight registrations (the JS worker rebuilds them
/// from scratch on every `CSS.highlights`/`Highlight` change).
pub(crate) fn prim_clear_highlights(_scope: &mut Ctx, _args: NativeArgs, _rv: &mut NativeReturn) {
    crate::clear_active_highlights();
}

/// `__addHighlight(name, nodeId)` — register that `nodeId` is covered by the highlight `name`
/// (its `::highlight(name)` pseudo applies). Called once per node in each registered highlight's
/// ranges.
pub(crate) fn prim_add_highlight(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let name = arg_str(scope, &args, 0);
    if let Some(node) = arg_node(scope, &args, 1) {
        if !name.is_empty() {
            crate::add_active_highlight(node.0, name);
        }
    }
}

pub(crate) fn prim_observers_active(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let active = args.get(0).boolean_value(scope);
    let state = host_state(scope);
    state.observers_active.set(active);
    if !active {
        // No observers left: drop any pending records so they don't leak into a later session.
        state.mutations.borrow_mut().clear();
    }
}

/// `__drainMutations() -> string` — returns the queued mutation records as a JSON array and
/// clears the queue. Each record: `{kind,target,attr,oldValue,added:[ids],removed:[ids]}`.
pub(crate) fn prim_drain_mutations(scope: &mut Ctx, _args: NativeArgs, rv: &mut NativeReturn) {
    let state = host_state(scope);
    let recs = std::mem::take(&mut *state.mutations.borrow_mut());
    let mut json = String::from("[");
    for (i, r) in recs.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        json.push_str("{\"kind\":");
        json.push_str(&js_string_literal(r.kind));
        json.push_str(",\"target\":");
        json.push_str(&r.target.0.to_string());
        json.push_str(",\"attr\":");
        match &r.attr_name {
            Some(a) => json.push_str(&js_string_literal(a)),
            None => json.push_str("null"),
        }
        json.push_str(",\"oldValue\":");
        match &r.old_value {
            Some(v) => json.push_str(&js_string_literal(v)),
            None => json.push_str("null"),
        }
        json.push_str(",\"added\":[");
        for (j, id) in r.added.iter().enumerate() {
            if j > 0 {
                json.push(',');
            }
            json.push_str(&id.0.to_string());
        }
        json.push_str("],\"removed\":[");
        for (j, id) in r.removed.iter().enumerate() {
            if j > 0 {
                json.push(',');
            }
            json.push_str(&id.0.to_string());
        }
        json.push_str("]}");
    }
    json.push(']');
    let s = js_str(scope, &json);
    rv.set(s);
}

/// Canonicalize numeric tokens while retaining quoted text and identifier boundaries.
/// Work on bytes because the syntax being recognized is ASCII; non-ASCII bytes are copied verbatim.
fn normalize_css_numbers(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        if matches!(ch, b'\"' | b'\'') {
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != ch {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 1;
                }
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            out.extend_from_slice(&bytes[start..i]);
            continue;
        }
        let mut end = i;
        if matches!(bytes[end], b'+' | b'-') {
            end += 1;
        }
        let body_start = end;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        let whole_digits = end - body_start;
        if end < bytes.len() && bytes[end] == b'.' {
            end += 1;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
        }
        let has_number = whole_digits > 0 || (end > body_start + 1 && bytes[body_start] == b'.');
        let follows_ident = i > 0 && (bytes[i - 1].is_ascii_alphabetic() || bytes[i - 1] == b'_');
        if has_number && !follows_ident {
            let mut body_end = end;
            if bytes[body_start..end].contains(&b'.') {
                while body_end > body_start && bytes[body_end - 1] == b'0' {
                    body_end -= 1;
                }
                if body_end > body_start && bytes[body_end - 1] == b'.' {
                    body_end -= 1;
                }
            }
            let body = &bytes[body_start..body_end];
            let bare_fraction = bytes[body_start] == b'.';
            if ch == b'-' && !(body == b"0" || (bare_fraction && body.is_empty())) {
                out.push(b'-');
            }
            if bare_fraction {
                out.push(b'0');
            }
            out.extend_from_slice(body);
            i = end;
        } else {
            out.push(ch);
            i += 1;
        }
    }
    // Only complete ASCII tokens changed, so the original UTF-8 boundaries remain intact.
    String::from_utf8(out).expect("CSS numeric normalization preserves UTF-8")
}

fn prim_normalize_css_numbers(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let input = arg_str(scope, &args, 0);
    let normalized = normalize_css_numbers(&input);
    rv.set(js_str(scope, &normalized));
}

/// Install the node-id DOM primitives onto `globalThis`.
pub(crate) fn install_dom_primitives(scope: &mut Ctx, global: &Value) {
    if scope
        .realm_value_for_host(global, "browser.array")
        .is_none()
    {
        let array = scope
            .member_get(global, "Array")
            .unwrap_or_else(|_| panic!("Array intrinsic"));
        let harness = scope
            .member_get(global, "$262")
            .unwrap_or_else(|_| panic!("Lumen host intrinsic"));
        let detach = scope
            .member_get(&harness, "detachArrayBuffer")
            .unwrap_or_else(|_| panic!("detach intrinsic"));
        let object = scope
            .member_get(global, "Object")
            .unwrap_or_else(|_| panic!("Object intrinsic"));
        let descriptor = scope
            .member_get(&object, "getOwnPropertyDescriptor")
            .unwrap_or_else(|_| panic!("descriptor intrinsic"));
        let ab = scope
            .member_get(global, "ArrayBuffer")
            .unwrap_or_else(|_| panic!("ArrayBuffer intrinsic"));
        let proto = scope
            .member_get(&ab, "prototype")
            .unwrap_or_else(|_| panic!("ArrayBuffer prototype"));
        let desc = scope
            .invoke(descriptor, object, &[proto, Value::str("byteLength")])
            .unwrap_or_else(|_| panic!("ArrayBuffer descriptor"));
        let array_buffer_byte_length = scope
            .member_get(&desc, "get")
            .unwrap_or_else(|_| panic!("ArrayBuffer getter"));
        scope.set_realm_value_for_host(global, "browser.array", array);
        scope.set_realm_value_for_host(global, "browser.detach", detach);
        scope.set_realm_value_for_host(
            global,
            "browser.arrayBufferByteLength",
            array_buffer_byte_length,
        );
    }
    set_fn(
        scope,
        global,
        "__normalizeCssNumbers",
        prim_normalize_css_numbers,
    );
    set_fn(scope, global, "__createElement", prim_create_element);
    set_fn(scope, global, "__createText", prim_create_text);
    set_fn(scope, global, "__createComment", prim_create_comment);
    set_fn(scope, global, "__createCData", prim_create_cdata);
    set_fn(
        scope,
        global,
        "__createDocumentNode",
        prim_create_document_node,
    );
    set_fn(
        scope,
        global,
        "__createDocumentFragment",
        prim_create_document_fragment,
    );
    set_fn(
        scope,
        global,
        "__createDocumentType",
        prim_create_document_type,
    );
    set_fn(
        scope,
        global,
        "__createProcessingInstruction",
        prim_create_processing_instruction,
    );
    set_fn(scope, global, "__doctypeInfo", prim_doctype_info);
    set_fn(scope, global, "__piTarget", prim_pi_target);
    set_fn(scope, global, "__cloneNode", prim_clone_node);
    set_fn(scope, global, "__getAttr", prim_get_attr);
    set_fn(scope, global, "__setAttr", prim_set_attr);
    set_fn(scope, global, "__removeAttr", prim_remove_attr);
    set_fn(scope, global, "__attrNames", prim_attr_names);
    set_fn(scope, global, "__storageLoad", prim_storage_load);
    set_fn(scope, global, "__storageSave", prim_storage_save);
    set_fn(scope, global, "__cookie", prim_cookie);
    set_fn(scope, global, "__setCookie", prim_set_cookie);
    set_fn(scope, global, "__cryptoRandom", prim_crypto_random);
    set_fn(scope, global, "__scrollY", prim_scroll_y);
    set_fn(scope, global, "__prefersDark", prim_prefers_dark);
    set_fn(scope, global, "__scrollSet", prim_scroll_set);
    set_fn(scope, global, "__navigate", prim_navigate);
    set_fn(scope, global, "__focusedNode", prim_focused_node);
    set_fn(scope, global, "__scrollIntoView", prim_scroll_into_view);
    set_fn(scope, global, "__appendChild", prim_append_child);
    set_fn(scope, global, "__insertBefore", prim_insert_before);
    set_fn(scope, global, "__removeChild", prim_remove_child);
    set_fn(scope, global, "__children", prim_children);
    set_fn(scope, global, "__parent", prim_parent);
    set_fn(scope, global, "__tag", prim_tag);
    set_fn(scope, global, "__namespaceUri", prim_namespace_uri);
    set_fn(scope, global, "__nodeType", prim_node_type);
    set_fn(scope, global, "__rect", prim_rect);
    set_fn(scope, global, "__naturalSize", prim_natural_size);
    set_fn(
        scope,
        global,
        "__detachArrayBuffer",
        prim_detach_array_buffer,
    );
    set_fn(scope, global, "__canvasPixels", prim_canvas_pixels);
    set_fn(scope, global, "__rasterizeCanvas", prim_rasterize_canvas);
    set_fn(scope, global, "__elemMetrics", prim_elem_metrics);
    set_fn(scope, global, "__textContent", prim_text_content);
    set_fn(scope, global, "__setTextContent", prim_set_text_content);
    set_fn(scope, global, "__innerHTML", prim_inner_html);
    set_fn(scope, global, "__outerHTML", prim_outer_html);
    set_fn(scope, global, "__setInnerHTML", prim_set_inner_html);
    set_fn(
        scope,
        global,
        "__parseHtmlSections",
        prim_parse_html_sections,
    );
    set_fn(scope, global, "__innerText", prim_inner_text);
    set_fn(scope, global, "__setInnerText", prim_set_inner_text);
    set_fn(scope, global, "__setOuterText", prim_set_outer_text);
    set_fn(scope, global, "__getElementById", prim_get_element_by_id);
    set_fn(scope, global, "__querySelectorAll", prim_query_selector_all);
    set_fn(
        scope,
        global,
        "__querySelectorAllWithin",
        prim_query_selector_all_within,
    );
    set_fn(
        scope,
        global,
        "__getElementsByTagName",
        prim_get_elements_by_tag_name,
    );
    set_fn(
        scope,
        global,
        "__getElementsByTagNameWithin",
        prim_get_elements_by_tag_name_within,
    );
    set_fn(
        scope,
        global,
        "__getElementsByClassName",
        prim_get_elements_by_class_name,
    );
    set_fn(
        scope,
        global,
        "__documentElementId",
        prim_document_element_id,
    );
    set_fn(scope, global, "__documentRootId", prim_document_root_id);
    set_fn(scope, global, "__bodyId", prim_body_id);
    set_fn(scope, global, "__headId", prim_head_id);
    set_fn(scope, global, "__rootId", prim_root_id);
    set_fn(scope, global, "__titleText", prim_title_text);
    set_fn(scope, global, "__urlParse", prim_url_parse);
    set_fn(scope, global, "__urlSet", prim_url_set);
    set_fn(scope, global, "__formDecode", prim_form_decode);
    set_fn(scope, global, "__fetch", prim_fetch);
    set_fn(scope, global, "__request", prim_request);
    set_fn(scope, global, "__startFetch", prim_start_fetch);
    set_fn(scope, global, "__wsConnect", prim_ws_connect);
    set_fn(scope, global, "__wsSend", prim_ws_send);
    set_fn(scope, global, "__wsClose", prim_ws_close);
    set_fn(scope, global, "__observersActive", prim_observers_active);
    set_fn(scope, global, "__commitSelection", prim_commit_selection);
    set_fn(scope, global, "__clearHighlights", prim_clear_highlights);
    set_fn(scope, global, "__addHighlight", prim_add_highlight);
    set_fn(scope, global, "__drainMutations", prim_drain_mutations);
    set_fn(
        scope,
        global,
        "__computedStyleProp",
        prim_computed_style_prop,
    );
    set_fn(
        scope,
        global,
        "__computedStyleNames",
        prim_computed_style_names,
    );
    // Dedicated-worker bridge natives (create a worker context, post across the page<->worker
    // boundary, run worker scripts at top level). Installed on every context so workers can spawn
    // sub-workers.
    crate::worker::register_worker_natives(scope, global);
    // Iframe browsing-context bridge natives (load a frame document into its own context, post
    // across the frame<->parent boundary). Installed on every context so nested iframes work.
    crate::register_iframe_natives(scope, global);
}

#[cfg(test)]
mod lumen_tests {
    use super::normalize_css_numbers;
    use crate::*;

    #[test]
    fn css_numeric_normalization_preserves_strings_and_units() {
        assert_eq!(
            normalize_css_numbers("+.50px -0.00 2.0 0001 a01 url(\"002.50\") '1.00' 中文.5"),
            "0.5px 0 2 0001 a01 url(\"002.50\") '1.00' 中文0.5"
        );
        assert_eq!(
            normalize_css_numbers("-.0 -.50 +.00 -00.00 1. +. -"),
            "0 -0.5 0 -00 1 +. -"
        );
    }

    #[test]
    fn rust_string_boundary_preserves_astral_private_use_characters() {
        let (_, output) = run_with_dom(
            html::parse("<body></body>"),
            vec![r#"
                document.body.textContent = String.fromCodePoint(0x10f800, 0x10ffff, 0x1f600);
                console.log(document.body.textContent);
                location.hash = String.fromCodePoint(0x10ffff);
                console.log(location.hash);
            "#
            .to_string()],
            "https://example.test/",
        );
        assert!(output[0].error.is_none(), "{:?}", output[0].error);
        assert_eq!(
            output[0].console,
            ["\u{10f800}\u{10ffff}\u{1f600}", "#%F4%8F%BF%BF"]
        );
    }

    #[test]
    fn dom_arrays_use_the_realms_original_constructor() {
        let (_, output) = run_with_dom(
            html::parse("<body><p>one</p><p>two</p></body>"),
            vec![r#"
                var OriginalArray = Array;
                var bodyId = document.body.__node;
                globalThis.Array = function () { throw new Error('replaced Array'); };
                var children = __children(bodyId);
                globalThis.Array = OriginalArray;
                console.log(OriginalArray.isArray(children), children.length);
            "#
            .to_string()],
            "https://example.test/",
        );
        assert!(output[0].error.is_none(), "{:?}", output[0].error);
        assert_eq!(output[0].console, ["true 2"]);
    }

    #[test]
    fn detach_requires_a_real_array_buffer_and_detaches_views() {
        let (_, output) = run_with_dom(
            html::parse("<body></body>"),
            vec![r#"
                var buffer = new ArrayBuffer(4);
                var view = new Uint8Array(buffer);
                console.log(__detachArrayBuffer({byteLength: 4}));
                console.log(__detachArrayBuffer(buffer), buffer.byteLength, view.length);
            "#
            .to_string()],
            "https://example.test/",
        );
        assert!(output[0].error.is_none(), "{:?}", output[0].error);
        assert_eq!(output[0].console, ["false", "true 0 0"]);
    }
}
