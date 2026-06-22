use crate::*;

/// Render a V8 value to a display string (via JS `String(value)` coercion). Never throws out:
/// uses `to_rust_string_lossy` after a `to_string` coercion, falling back to "undefined".
pub(crate) fn render_value(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> String {
    match value.to_string(scope) {
        Some(s) => s.to_rust_string_lossy(scope),
        None => "undefined".to_string(),
    }
}

/// Read positional argument `i` from a callback as a Rust string (JS-coerced). Missing → "".
pub(crate) fn arg_str(
    scope: &mut v8::PinScope,
    args: &v8::FunctionCallbackArguments,
    i: i32,
) -> String {
    if i >= args.length() {
        return String::new();
    }
    let v = args.get(i);
    render_value(scope, v)
}

/// Read positional argument `i` as a node id (`usize`). Missing/NaN → None.
pub(crate) fn arg_node(
    scope: &mut v8::PinScope,
    args: &v8::FunctionCallbackArguments,
    i: i32,
) -> Option<dom::NodeId> {
    if i >= args.length() {
        return None;
    }
    let v = args.get(i);
    let n = v.number_value(scope)?;
    if n.is_nan() || n < 0.0 {
        return None;
    }
    let id = dom::NodeId(n as usize);
    // Reject ids outside the live arena. Valid node ids are always `< len` (the arena only grows,
    // never reuses slots), so a stale or garbage id from page JS — which would otherwise be pushed
    // into a children list and later panic the renderer with an out-of-bounds index — is dropped
    // here. Callers treat `None` as "no such node" and skip the operation.
    if id.0 >= host_state(scope).doc.borrow().len() {
        return None;
    }
    Some(id)
}

/// Build a JS string Local. Falls back to an empty string if V8 rejects the (huge) input.
pub(crate) fn js_str<'s>(scope: &mut v8::PinScope<'s, '_>, s: &str) -> v8::Local<'s, v8::Value> {
    match v8::String::new(scope, s) {
        Some(v) => v.into(),
        None => v8::String::empty(scope).into(),
    }
}

/// Build a JS array of node ids (as numbers).
pub(crate) fn js_id_array<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ids: &[dom::NodeId],
) -> v8::Local<'s, v8::Value> {
    let elements: Vec<v8::Local<v8::Value>> = ids
        .iter()
        .map(|id| v8::Number::new(scope, id.0 as f64).into())
        .collect();
    v8::Array::new_with_elements(scope, &elements).into()
}

/// Build a JS array of strings.
pub(crate) fn js_str_array<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    items: &[String],
) -> v8::Local<'s, v8::Value> {
    let elements: Vec<v8::Local<v8::Value>> = items.iter().map(|s| js_str(scope, s)).collect();
    v8::Array::new_with_elements(scope, &elements).into()
}

// ---------------------------------------------------------------------------------------------
// Native primitive callbacks. These are bare functions (V8 callbacks cannot capture state); they
// recover the shared DOM + console from the context slot via `host_state(scope)`. The JS
// bootstrap (DOCUMENT_BOOTSTRAP) builds `document`/element objects on top of these.
// ---------------------------------------------------------------------------------------------

/// `__consoleLog(...args)` — push a space-joined line into the shared console buffer.
pub(crate) fn prim_console_log(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let mut parts = Vec::with_capacity(args.length() as usize);
    for i in 0..args.length() {
        let v = args.get(i);
        parts.push(render_value(scope, v));
    }
    let line = parts.join(" ");
    host_state(scope).console.borrow_mut().push(line);
}

/// `__createElement(tag) -> id`
pub(crate) fn prim_create_element(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
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
pub(crate) fn prim_create_text(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let text = arg_str(scope, &args, 0);
    let state = host_state(scope);
    let id = state
        .doc
        .borrow_mut()
        .alloc(dom::NodeData::Text(text), None);
    rv.set_double(id.0 as f64);
}

/// `__createComment(text) -> id`
pub(crate) fn prim_create_comment(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let text = arg_str(scope, &args, 0);
    let state = host_state(scope);
    let id = state
        .doc
        .borrow_mut()
        .alloc(dom::NodeData::Comment(text), None);
    rv.set_double(id.0 as f64);
}

/// `__createCData(text) -> id` — a parentless CDATASection node.
pub(crate) fn prim_create_cdata(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
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
pub(crate) fn prim_create_document_node(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let state = host_state(scope);
    let id = state.doc.borrow_mut().alloc(dom::NodeData::Document, None);
    rv.set_double(id.0 as f64);
}

/// `__createDocumentFragment() -> id` — a parentless `DocumentFragment` arena node.
pub(crate) fn prim_create_document_fragment(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let state = host_state(scope);
    let id = state
        .doc
        .borrow_mut()
        .alloc(dom::NodeData::DocumentFragment, None);
    rv.set_double(id.0 as f64);
}

/// `__createDocumentType(name, publicId, systemId) -> id` — a parentless `DocumentType` node.
pub(crate) fn prim_create_document_type(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
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
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
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
pub(crate) fn prim_doctype_info(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
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
            let obj = v8::Object::new(scope);
            let k = v8::String::new(scope, "name").unwrap();
            let v = js_str(scope, &name);
            obj.set(scope, k.into(), v);
            let k = v8::String::new(scope, "publicId").unwrap();
            let v = js_str(scope, &public_id);
            obj.set(scope, k.into(), v);
            let k = v8::String::new(scope, "systemId").unwrap();
            let v = js_str(scope, &system_id);
            obj.set(scope, k.into(), v);
            rv.set(obj.into());
        }
        None => rv.set_null(),
    }
}

/// `__piTarget(id) -> string | null` — a ProcessingInstruction node's target.
pub(crate) fn prim_pi_target(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
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
pub(crate) fn prim_clone_node(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
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
pub(crate) fn prim_get_attr(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
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
// Limitation: only inline `<style>` blocks (and the UA sheet that `cascade` auto-prepends) are
// honoured. External `<link rel=stylesheet>` CSS is not available in-Session (the engine fetches it
// out of band and it isn't surfaced to the worker), so author rules from external sheets do not
// affect these computed values. That's fine for feature-detection, which probes inline styles on
// throwaway elements — it never relies on external CSS.
// ---------------------------------------------------------------------------------------------

/// Walk the document for `<style>` elements, concatenate their text content, and parse it into a
/// single author stylesheet. Returns an empty `Vec` when there are no `<style>` blocks (the cascade
/// then runs with just the UA sheet + inline `style=""` attributes).
/// Join `href` onto `base` (both treated as URLs); returns `base` if either fails to parse.
pub(crate) fn join_url(base: &str, href: &str) -> String {
    match url::Url::parse(base).and_then(|b| b.join(href.trim())) {
        Ok(u) => u.into(),
        Err(_) => base.to_string(),
    }
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
                        if let Some(css) = fetch_link_css(href, fetcher) {
                            // The link sheet's rules resolve against the link's own (absolute) URL.
                            out.push(SheetEntry {
                                title: e.attrs.get("title").cloned(),
                                alternate: is_alternate,
                                css,
                                seq: id.0,
                                base: join_url(doc_base, href),
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
pub(crate) fn prim_storage_load(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let key = arg_str(scope, &args, 0);
    let content = std::fs::read_to_string(storage_path(&key)).unwrap_or_default();
    let s = js_str(scope, &content);
    rv.set(s);
}

/// `__storageSave(key, json)` — persist `json` for `key` (localStorage write-through).
pub(crate) fn prim_storage_save(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let key = arg_str(scope, &args, 0);
    let json = arg_str(scope, &args, 1);
    let _ = std::fs::create_dir_all(storage_dir());
    let _ = std::fs::write(storage_path(&key), json);
}

/// `__scrollY() -> number` — the page's current vertical scroll offset (CSS px), so `window.scrollY`
/// / `pageYOffset` / `scrollingElement.scrollTop` report the real position the engine is showing.
pub(crate) fn prim_scroll_y(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let y = host_state(scope).viewport_scroll_y.get();
    rv.set(v8::Number::new(scope, y as f64).into());
}

/// `__prefersDark() -> boolean` — whether the effective OS appearance is Dark, read live from the
/// process-global flag the engine sets (`set_color_scheme_dark`). Drives the JS `matchMedia`
/// `prefers-color-scheme` feature so it tracks the real macOS Light/Dark setting.
pub(crate) fn prim_prefers_dark(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    rv.set(v8::Boolean::new(scope, color_scheme_dark()).into());
}

/// A JS-requested scroll target (document CSS px), read+cleared by the engine after each Session
/// interaction. `i64::MIN` = no request. Process-global: the active tab is the one being driven.
pub(crate) static PENDING_SCROLL: AtomicI64 = AtomicI64::new(i64::MIN);

/// Read + clear a pending JS scroll request (`window.scrollTo` / `scrollIntoView`). The engine calls
/// this after `tick`/`dispatch_*`/`console_eval` and applies it to its scroll offset.
pub fn take_pending_scroll() -> Option<f32> {
    let v = PENDING_SCROLL.swap(i64::MIN, Ordering::AcqRel);
    if v == i64::MIN {
        None
    } else {
        Some(v as f32)
    }
}

/// `__scrollSet(y)` — request a scroll to document `y` (CSS px); clamped to >= 0.
pub(crate) fn prim_scroll_set(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let y = args.get(0).number_value(scope).unwrap_or(0.0);
    let y = if y.is_finite() { y.max(0.0) } else { 0.0 };
    PENDING_SCROLL.store(y.round() as i64, Ordering::Release);
}

/// `__scrollIntoView(id)` — request a scroll so node `id`'s top is near the viewport top.
pub(crate) fn prim_scroll_into_view(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
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
        PENDING_SCROLL.store(t.round() as i64, Ordering::Release);
    }
}

/// Fill `buf` with cryptographically-random bytes from the OS (`/dev/urandom`), falling back to a
/// time/address-seeded PRNG only if that's unreadable.
pub(crate) fn fill_random(buf: &mut [u8]) {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(buf).is_ok() {
            return;
        }
    }
    use std::hash::{BuildHasher, Hasher};
    let mut seed = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    for b in buf.iter_mut() {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *b = (seed >> 33) as u8;
    }
}

/// `__cryptoRandom(n) -> [byte, ...]` — `n` real random bytes (for `crypto.getRandomValues`/UUID).
pub(crate) fn prim_crypto_random(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let n = args.get(0).number_value(scope).unwrap_or(0.0);
    let n = if n.is_finite() && n > 0.0 {
        (n as usize).min(1 << 20)
    } else {
        0
    };
    let mut buf = vec![0u8; n];
    fill_random(&mut buf);
    let arr = v8::Array::new(scope, n as i32);
    for (i, &b) in buf.iter().enumerate() {
        let v = v8::Integer::new_from_unsigned(scope, b as u32);
        arr.set_index(scope, i as u32, v.into());
    }
    rv.set(arr.into());
}

/// `__appendChild(parentId, childId)` — reparent child under parent.
pub(crate) fn prim_append_child(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
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
pub(crate) fn prim_insert_before(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
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
pub(crate) fn prim_remove_child(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
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
pub(crate) fn prim_children(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let ids: Vec<dom::NodeId> = node
        .map(|n| state.doc.borrow().get(n).children.clone())
        .unwrap_or_default();
    let arr = js_id_array(scope, &ids);
    rv.set(arr);
}

/// `__parent(id) -> id | -1`
pub(crate) fn prim_parent(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let parent = node.and_then(|n| state.doc.borrow().get(n).parent);
    rv.set_double(parent.map(|p| p.0 as f64).unwrap_or(-1.0));
}

/// `__tag(id) -> string` (lowercased), or "" for non-elements.
pub(crate) fn prim_tag(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
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
pub(crate) fn prim_namespace_uri(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
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
pub(crate) fn prim_node_type(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
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
pub(crate) fn prim_rect(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
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
    let obj = v8::Object::new(scope);
    let put = |k: &str, v: f32| {
        let key = v8::String::new(scope, k).unwrap();
        let val = v8::Number::new(scope, v as f64);
        obj.set(scope, key.into(), val.into());
    };
    put("x", left);
    put("y", top);
    put("left", left);
    put("top", top);
    put("right", left + w);
    put("bottom", top + h);
    put("width", w);
    put("height", h);
    rv.set(obj.into());
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
pub(crate) fn prim_canvas_pixels(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
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
    let obj = v8::Object::new(scope);
    let put_num = |scope: &mut v8::PinScope, obj: v8::Local<v8::Object>, k: &str, v: f64| {
        let key = v8::String::new(scope, k).unwrap();
        let val = v8::Number::new(scope, v);
        obj.set(scope, key.into(), val.into());
    };
    put_num(scope, obj, "w", sw as f64);
    put_num(scope, obj, "h", sh as f64);
    let b64 = base64_encode(&out);
    let key = v8::String::new(scope, "b64").unwrap();
    let val = js_str(scope, &b64);
    obj.set(scope, key.into(), val);
    rv.set(obj.into());
}

/// `__naturalSize(id) -> { w, h }`
///
/// The decoded intrinsic size of an `<img>` (CSS px), pushed by the engine alongside the layout
/// rects from its decoded-bitmap table. Backs `img.naturalWidth` / `img.naturalHeight`. A
/// missing/broken/not-yet-decoded image has no entry and reports `{ w: 0, h: 0 }`.
pub(crate) fn prim_natural_size(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let (w, h) = node
        .and_then(|n| state.image_natural.borrow().get(&n.0).copied())
        .unwrap_or((0.0, 0.0));
    let obj = v8::Object::new(scope);
    let put = |k: &str, v: f32| {
        let key = v8::String::new(scope, k).unwrap();
        let val = v8::Number::new(scope, v as f64);
        obj.set(scope, key.into(), val.into());
    };
    put("w", w);
    put("h", h);
    rv.set(obj.into());
}

/// `__elemMetrics(id) -> { ow, oh, ot, ol, sw, sh } | null`
///
/// Box metrics for `offsetWidth/Height/Top/Left`, `clientWidth/Height`, `scrollWidth/Height`:
/// - `ow`/`oh` = border-box width/height (offsetWidth/Height; clientWidth/Height ≈ same — we do
///   not subtract borders/scrollbars).
/// - `ot`/`ol` = document-absolute top/left (offsetTop/Left — a simplification; real offsetTop is
///   relative to `offsetParent`, but we report absolute coordinates).
/// - `sw`/`sh` = scrollWidth/Height ≈ the border-box size (no overflow tracking), EXCEPT for the
///   document root / `<html>` / `<body>`, where `sh` is the full document height and `sw` the
///   viewport width — so `document.documentElement.scrollHeight` reports the whole page.
/// Returns `null` when the node has no laid-out box.
pub(crate) fn prim_elem_metrics(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let rect = node.and_then(|n| state.layout_rects.borrow().get(&n.0).copied());
    // Client box (padding box: content + padding, excluding borders) computed from the cascade.
    // `clientWidth`/`clientHeight` exclude borders/scrollbars, so they cannot reuse the border-box
    // `ow`/`oh`.
    let client_box = node.and_then(|n| {
        with_cascade_map(&state, |_doc, map| {
            map.get(&n).map(|cs| {
                let cw = cs.width.unwrap_or(0.0) + cs.padding.left + cs.padding.right;
                let ch = cs.height.unwrap_or(0.0) + cs.padding.top + cs.padding.bottom;
                (cw, ch)
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
        (w, h)
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
    let obj = v8::Object::new(scope);
    let put = |k: &str, v: f32| {
        let key = v8::String::new(scope, k).unwrap();
        let val = v8::Number::new(scope, v as f64);
        obj.set(scope, key.into(), val.into());
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
    rv.set(obj.into());
}

/// `__textContent(id) -> string`
pub(crate) fn prim_text_content(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let s = node
        .map(|n| text_content(&state.doc.borrow(), n))
        .unwrap_or_default();
    let v = js_str(scope, &s);
    rv.set(v);
}

/// `__setTextContent(id, text)`
pub(crate) fn prim_set_text_content(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
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
pub(crate) fn prim_inner_html(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let s = node
        .map(|n| inner_html(&state.doc.borrow(), n))
        .unwrap_or_default();
    let v = js_str(scope, &s);
    rv.set(v);
}

/// `__setInnerHTML(id, html)`
pub(crate) fn prim_set_inner_html(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let html = arg_str(scope, &args, 1);
    let state = host_state(scope);
    if let Some(n) = node {
        state.bump_dom_version(); // invalidate getComputedStyle cache (subtree replaced)
        set_inner_html(&mut state.doc.borrow_mut(), n, &html);
    }
}

/// `__innerText(id) -> string` — the `innerText`/`outerText` getter (rendered-text algorithm).
pub(crate) fn prim_inner_text(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
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
pub(crate) fn prim_set_inner_text(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
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
pub(crate) fn prim_set_outer_text(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
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
pub(crate) fn prim_get_element_by_id(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let id = arg_str(scope, &args, 0);
    let state = host_state(scope);
    let found = {
        let d = state.doc.borrow();
        find_by_id(&d, d.root(), &id)
    };
    rv.set_double(found.map(|n| n.0 as f64).unwrap_or(-1.0));
}

/// `__querySelectorAll(sel) -> [id...]`
pub(crate) fn prim_query_selector_all(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
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
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
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
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
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
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
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
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
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

/// `__documentElementId() -> id | -1` (the <html> element)
pub(crate) fn prim_document_element_id(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let state = host_state(scope);
    let found = {
        let d = state.doc.borrow();
        find_by_tag(&d, d.root(), "html")
    };
    rv.set_double(found.map(|n| n.0 as f64).unwrap_or(-1.0));
}

/// `__documentRootId() -> id` — the Document node itself (arena root). Always valid, even when the
/// root element has been replaced/removed, so JS can find the document's children directly.
pub(crate) fn prim_document_root_id(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let root = host_state(scope).doc.borrow().root();
    rv.set_double(root.0 as f64);
}

/// `__bodyId() -> id | -1`
pub(crate) fn prim_body_id(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let state = host_state(scope);
    let found = {
        let d = state.doc.borrow();
        find_by_tag(&d, d.root(), "body")
    };
    rv.set_double(found.map(|n| n.0 as f64).unwrap_or(-1.0));
}

/// `__headId() -> id | -1`
pub(crate) fn prim_head_id(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let state = host_state(scope);
    let found = {
        let d = state.doc.borrow();
        find_by_tag(&d, d.root(), "head")
    };
    rv.set_double(found.map(|n| n.0 as f64).unwrap_or(-1.0));
}

/// `__rootId() -> id` (the Document root node)
pub(crate) fn prim_root_id(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let state = host_state(scope);
    let root = state.doc.borrow().root();
    rv.set_double(root.0 as f64);
}

/// `__titleText() -> string`
pub(crate) fn prim_title_text(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
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

/// `__fetch(url) -> string | null`
///
/// Synchronous network primitive backing JS `fetch()`. Resolves `url` against `globalThis.__pageURL`
/// (absolute URLs pass through unchanged; relative ones are joined onto the page URL), calls the host
/// fetcher, and returns the response body as a string. Returns `null` (JS) on any failure — bad URL,
/// no fetcher result, etc. Runs on the isolate's own worker thread, so the blocking host fetch is
/// fine. Never panics.
pub(crate) fn prim_fetch(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let raw = arg_str(scope, &args, 0);
    // Resolve against the page URL when present (so relative URLs work like other fetches).
    let resolved = {
        let global = scope.get_current_context().global(scope);
        let key = v8::String::new(scope, "__pageURL").unwrap();
        let base = global
            .get(scope, key.into())
            .filter(|v| v.is_string())
            .map(|v| v.to_rust_string_lossy(scope));
        match base {
            Some(b) if !b.is_empty() => match url::Url::parse(&b).and_then(|u| u.join(&raw)) {
                Ok(u) => u.to_string(),
                // Join failed: fall back to the raw URL (likely already absolute).
                Err(_) => raw.clone(),
            },
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
pub(crate) fn prim_request(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let method = arg_str(scope, &args, 0);
    let raw = arg_str(scope, &args, 1);
    let body = arg_str(scope, &args, 2);
    let headers_json = arg_str(scope, &args, 3);
    // Resolve against the page URL when present (so relative URLs work like other fetches).
    let resolved = {
        let global = scope.get_current_context().global(scope);
        let key = v8::String::new(scope, "__pageURL").unwrap();
        let base = global
            .get(scope, key.into())
            .filter(|v| v.is_string())
            .map(|v| v.to_rust_string_lossy(scope));
        match base {
            Some(b) if !b.is_empty() => match url::Url::parse(&b).and_then(|u| u.join(&raw)) {
                Ok(u) => u.to_string(),
                // Join failed: fall back to the raw URL (likely already absolute).
                Err(_) => raw.clone(),
            },
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
/// touches V8; the promise is settled later on the worker thread inside [`drain_event_loop`] via
/// `__resolveFetch`/`__rejectFetch`. Increments the in-flight counter so the drain keeps looping
/// until this completion is pulled.
pub(crate) fn prim_start_fetch(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let method = arg_str(scope, &args, 0);
    let raw = arg_str(scope, &args, 1);
    let body = arg_str(scope, &args, 2);
    let headers_json = arg_str(scope, &args, 3);
    // Resolve against the page URL when present (so relative URLs work like other fetches).
    let resolved = {
        let global = scope.get_current_context().global(scope);
        let key = v8::String::new(scope, "__pageURL").unwrap();
        let base = global
            .get(scope, key.into())
            .filter(|v| v.is_string())
            .map(|v| v.to_rust_string_lossy(scope));
        match base {
            Some(b) if !b.is_empty() => match url::Url::parse(&b).and_then(|u| u.join(&raw)) {
                Ok(u) => u.to_string(),
                Err(_) => raw.clone(),
            },
            _ => raw.clone(),
        }
    };

    let state = host_state(scope);
    let id = state.next_fetch_id.fetch_add(1, Ordering::Relaxed);
    let request_fetcher = Arc::clone(&state.request_fetcher);
    let tx = state.fetch_tx.clone();
    state.in_flight.set(state.in_flight.get() + 1);

    // Thread-per-request: sites fire a bounded handful concurrently, so this is fine. The work is
    // pure host I/O; it never re-enters V8. On spawn failure we synchronously deliver `None` so the
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

    rv.set(v8::Number::new(scope, id as f64).into());
}

/// `__wsConnect(url) -> id (number)`
///
/// Backs `new WebSocket(url)`. Allocates a socket id, then asks the host `ws_connector` to spawn a
/// background socket thread (which runs `net::ws_run`). On success the per-socket outgoing sender is
/// stored under the id so `__wsSend`/`__wsClose` can reach the socket; on failure we synthesize an
/// `error` (kind 4) + `close` (kind 3) event so the JS object still fires onerror/onclose. The id is
/// always returned so the JS object can register itself in `__wsRegistry` either way.
pub(crate) fn prim_ws_connect(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let raw = arg_str(scope, &args, 0);
    // Resolve against the page URL when present (so a relative ws path works), like fetch.
    let resolved = {
        let global = scope.get_current_context().global(scope);
        let key = v8::String::new(scope, "__pageURL").unwrap();
        let base = global
            .get(scope, key.into())
            .filter(|v| v.is_string())
            .map(|v| v.to_rust_string_lossy(scope));
        match base {
            Some(b) if !b.is_empty() => match url::Url::parse(&b).and_then(|u| u.join(&raw)) {
                Ok(u) => u.to_string(),
                Err(_) => raw.clone(),
            },
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
    rv.set(v8::Number::new(scope, id as f64).into());
}

/// `__wsSend(id, kind, payload)` — enqueue an outgoing frame on socket `id`. kind 0 = text,
/// 1 = binary (payload is base64). No-op if the id is unknown (already closed).
pub(crate) fn prim_ws_send(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
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
pub(crate) fn prim_ws_close(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
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
    scope: &mut v8::PinScope,
    target: v8::Local<v8::Object>,
    name: &str,
    cb: impl v8::MapFnTo<v8::FunctionCallback>,
) {
    let func = v8::Function::new(scope, cb).unwrap();
    let key = v8::String::new(scope, name).unwrap();
    target.set(scope, key.into(), func.into());
}

/// Install the `__consoleLog` native sink. The JS `console` object (built in the bootstrap) calls
/// it. On the no-DOM `Runtime::eval` path it is the only thing installed besides timers.
pub(crate) fn install_console_sink(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
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
pub(crate) fn prim_observers_active(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
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
pub(crate) fn prim_drain_mutations(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
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

/// Install the node-id DOM primitives onto `globalThis`.
pub(crate) fn install_dom_primitives(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
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
    set_fn(scope, global, "__cryptoRandom", prim_crypto_random);
    set_fn(scope, global, "__scrollY", prim_scroll_y);
    set_fn(scope, global, "__prefersDark", prim_prefers_dark);
    set_fn(scope, global, "__scrollSet", prim_scroll_set);
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
    set_fn(scope, global, "__canvasPixels", prim_canvas_pixels);
    set_fn(scope, global, "__elemMetrics", prim_elem_metrics);
    set_fn(scope, global, "__textContent", prim_text_content);
    set_fn(scope, global, "__setTextContent", prim_set_text_content);
    set_fn(scope, global, "__innerHTML", prim_inner_html);
    set_fn(scope, global, "__setInnerHTML", prim_set_inner_html);
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
    set_fn(scope, global, "__fetch", prim_fetch);
    set_fn(scope, global, "__request", prim_request);
    set_fn(scope, global, "__startFetch", prim_start_fetch);
    set_fn(scope, global, "__wsConnect", prim_ws_connect);
    set_fn(scope, global, "__wsSend", prim_ws_send);
    set_fn(scope, global, "__wsClose", prim_ws_close);
    set_fn(scope, global, "__observersActive", prim_observers_active);
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
}
