//! JavaScript runtime (Phase: scripting).
//!
//! Wraps Google's V8 JavaScript engine behind our own small API so the engine could be swapped
//! later — same pattern as `net`/ureq and `paint`/fontdue. Nothing outside this crate knows V8
//! exists. V8 gives us full JS speed plus complete language support, including real ES modules
//! and dynamic `import()`.
//!
//! ## V8 integration shape
//! - V8 is process-global-initialized exactly once (`std::sync::Once`).
//! - A V8 `Isolate` is single-thread-bound, so every entry point that owns an isolate either runs
//!   on the calling thread ([`Runtime`]) or creates the isolate on a dedicated worker thread
//!   ([`eval_batch`]/[`run_with_dom`]/[`run_modules`]).
//! - Native callbacks in V8 are bare C function pointers and cannot capture Rust state. We share
//!   the page DOM and console buffer with them through a [`HostState`] stored on the **context
//!   slot** (`Context::set_slot`/`get_slot`), retrieved inside each callback via
//!   `scope.get_current_context().get_slot::<HostState>()`. The DOM is only ever touched on the
//!   isolate's own thread, so `Rc<RefCell<dom::Document>>` is fine (no `Send` needed).
//!
//! ## DOM exposure
//! Rather than port dozens of bespoke per-node wrapper closures, we expose a *small* set of native
//! primitive functions on `globalThis`, keyed by integer node ids (`dom::NodeId` is a `usize`),
//! and build the `document`/element objects in JavaScript on top of them (the
//! `DOCUMENT_BOOTSTRAP`, `TIMERS_BOOTSTRAP`, and `BROWSER_ENV_BOOTSTRAP` strings). All the
//! framework-compatibility machinery (per-node wrapper cache + expandos, `style`/`classList`/
//! `dataset` write-through, the DOM interface class hierarchy + `instanceof`, navigator/location/
//! storage/observers, the timer/event loop) lives in that reused, engine-agnostic JavaScript.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Once;

/// A JS execution result: the value rendered as a string (if any) plus any console output
/// captured during execution.
#[derive(Debug, Default, Clone)]
pub struct EvalOutput {
    pub value: Option<String>,
    pub console: Vec<String>,
    pub error: Option<String>,
}

/// Initialize the V8 platform exactly once for the whole process. Safe to call repeatedly.
fn ensure_v8_initialized() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}

// ---------------------------------------------------------------------------------------------
// Shared host state (lives on the V8 context slot; retrieved inside native callbacks).
// ---------------------------------------------------------------------------------------------

/// A shared, mutable handle to the page's DOM.
type SharedDoc = Rc<RefCell<dom::Document>>;

/// State shared between Rust and the native primitive callbacks. Stored on the context slot as an
/// `Rc<HostState>` so any callback can recover it from `scope.get_current_context().get_slot()`.
/// Interior mutability via `RefCell` since the slot only hands out `&HostState` (well, `Rc`).
struct HostState {
    doc: SharedDoc,
    console: RefCell<Vec<String>>,
    /// Host network fetcher (the same one the engine passes into `run_modules`). Called on the
    /// isolate's own worker thread by the `__fetch` native primitive that backs JS `fetch()`.
    /// Blocking inside it is fine (single-threaded worker, synchronous drain model). The no-DOM
    /// paths install a no-op fetcher that always returns `None`. Held as an `Rc` so the module
    /// registry on the `run_modules` path can share the very same fetcher.
    fetcher: Rc<dyn Fn(&str) -> Option<String>>,
}

impl HostState {
    fn new(doc: SharedDoc) -> Rc<Self> {
        Self::with_fetcher(doc, Rc::new(|_| None))
    }

    fn with_fetcher(
        doc: SharedDoc,
        fetcher: Rc<dyn Fn(&str) -> Option<String>>,
    ) -> Rc<Self> {
        Rc::new(HostState { doc, console: RefCell::new(Vec::new()), fetcher })
    }
}

/// Recover the `Rc<HostState>` from the current context's slot. Panics only if state was never
/// installed, which is a programming error (every context we run callbacks in installs it).
fn host_state(scope: &mut v8::PinScope) -> Rc<HostState> {
    let context = scope.get_current_context();
    context
        .get_slot::<HostState>()
        .expect("HostState must be installed on the context")
}

// ---------------------------------------------------------------------------------------------
// DOM helpers (engine-agnostic; operate directly on `dom::Document`). Reused from the prior
// implementation — these are pure Rust and unchanged in behavior.
// ---------------------------------------------------------------------------------------------

/// Concatenate every descendant `Text` node under `id`, in document order.
fn text_content(doc: &dom::Document, id: dom::NodeId) -> String {
    let mut out = String::new();
    fn walk(doc: &dom::Document, id: dom::NodeId, out: &mut String) {
        match &doc.get(id).data {
            dom::NodeData::Text(t) => out.push_str(t),
            _ => {
                for &child in &doc.get(id).children {
                    walk(doc, child, out);
                }
            }
        }
    }
    walk(doc, id, &mut out);
    out
}

/// Serialize the children of `id` back to an HTML string (the `innerHTML` of `id`).
fn inner_html(doc: &dom::Document, id: dom::NodeId) -> String {
    fn is_void(tag: &str) -> bool {
        matches!(
            tag.to_ascii_lowercase().as_str(),
            "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" | "link"
                | "meta" | "param" | "source" | "track" | "wbr"
        )
    }
    fn escape_text(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }
    fn escape_attr(s: &str) -> String {
        s.replace('&', "&amp;").replace('"', "&quot;")
    }
    fn serialize_node(doc: &dom::Document, id: dom::NodeId, out: &mut String) {
        match &doc.get(id).data {
            dom::NodeData::Text(t) => out.push_str(&escape_text(t)),
            dom::NodeData::Comment(c) => {
                out.push_str("<!--");
                out.push_str(c);
                out.push_str("-->");
            }
            dom::NodeData::Element(e) => {
                out.push('<');
                out.push_str(&e.tag);
                for (k, v) in &e.attrs {
                    out.push(' ');
                    out.push_str(k);
                    out.push_str("=\"");
                    out.push_str(&escape_attr(v));
                    out.push('"');
                }
                out.push('>');
                if !is_void(&e.tag) {
                    for &child in &doc.get(id).children {
                        serialize_node(doc, child, out);
                    }
                    out.push_str("</");
                    out.push_str(&e.tag);
                    out.push('>');
                }
            }
            dom::NodeData::Document => {
                for &child in &doc.get(id).children {
                    serialize_node(doc, child, out);
                }
            }
        }
    }
    let mut out = String::new();
    for &child in &doc.get(id).children {
        serialize_node(doc, child, &mut out);
    }
    out
}

/// Replace all children of `id` with a single `Text` node holding `text`.
fn set_text_content(doc: &mut dom::Document, id: dom::NodeId, text: &str) {
    // For a Text/Comment node, mutating `.textContent`/`.data`/`.nodeValue` updates the node's own
    // string value in place (Vue's `setText` patches text/comment anchors this way).
    match &mut doc.get_mut(id).data {
        dom::NodeData::Text(t) => {
            *t = text.to_string();
            return;
        }
        dom::NodeData::Comment(c) => {
            *c = text.to_string();
            return;
        }
        _ => {}
    }
    let old: Vec<dom::NodeId> = std::mem::take(&mut doc.get_mut(id).children);
    for child in old {
        doc.get_mut(child).parent = None;
    }
    doc.append_child(id, dom::NodeData::Text(text.to_string()));
}

/// Parse `html` and replace `target`'s children with the resulting real nodes in the live `doc`.
fn set_inner_html(doc: &mut dom::Document, target: dom::NodeId, html: &str) {
    let old: Vec<dom::NodeId> = std::mem::take(&mut doc.get_mut(target).children);
    for child in old {
        doc.get_mut(child).parent = None;
    }
    let frag = html::parse(html);
    let frag_root = frag.root();
    copy_children_into(doc, target, &frag, frag_root);
}

/// Recursively copy the children of `src_node` (in `frag`) as children of `dst_parent` in `doc`.
/// Synthesized structural wrappers (`html`/`head`/`body`) are transparently descended into.
fn copy_children_into(
    doc: &mut dom::Document,
    dst_parent: dom::NodeId,
    frag: &dom::Document,
    src_node: dom::NodeId,
) {
    for &child in &frag.get(src_node).children {
        match &frag.get(child).data {
            dom::NodeData::Element(e) if matches!(e.tag.as_str(), "html" | "head" | "body") => {
                copy_children_into(doc, dst_parent, frag, child);
            }
            data => {
                let new_id = doc.append_child(dst_parent, data.clone());
                copy_children_into(doc, new_id, frag, child);
            }
        }
    }
}

/// Depth-first search for the first element whose tag equals `tag` (ASCII case-insensitive).
fn find_by_tag(doc: &dom::Document, root: dom::NodeId, tag: &str) -> Option<dom::NodeId> {
    if let dom::NodeData::Element(e) = &doc.get(root).data {
        if e.tag.eq_ignore_ascii_case(tag) {
            return Some(root);
        }
    }
    for &child in &doc.get(root).children {
        if let Some(found) = find_by_tag(doc, child, tag) {
            return Some(found);
        }
    }
    None
}

/// Collect every element matching `tag` (ASCII case-insensitive), document order.
fn collect_by_tag(doc: &dom::Document, root: dom::NodeId, tag: &str, out: &mut Vec<dom::NodeId>) {
    if let dom::NodeData::Element(e) = &doc.get(root).data {
        if e.tag.eq_ignore_ascii_case(tag) {
            out.push(root);
        }
    }
    let children = doc.get(root).children.clone();
    for child in children {
        collect_by_tag(doc, child, tag, out);
    }
}

/// Depth-first search for the first element with `id` equal to `id`.
fn find_by_id(doc: &dom::Document, root: dom::NodeId, id: &str) -> Option<dom::NodeId> {
    if let dom::NodeData::Element(e) = &doc.get(root).data {
        if e.id() == Some(id) {
            return Some(root);
        }
    }
    for &child in &doc.get(root).children {
        if let Some(found) = find_by_id(doc, child, id) {
            return Some(found);
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// CSS selector engine (type / .class / #id / compound / descendant). Reused verbatim.
// ---------------------------------------------------------------------------------------------

/// A single compound selector, e.g. `div.foo#bar`.
#[derive(Debug, Default, Clone)]
struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
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
        true
    }
}

/// Parse a single compound selector (no combinators).
fn parse_compound(s: &str) -> Option<Compound> {
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
                let start = i;
                while i < bytes.len() && !matches!(bytes[i], '.' | '#' | '[' | ':') {
                    i += 1;
                }
                let name: String = bytes[start..i].iter().collect();
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
                while i < bytes.len() && bytes[i] != ']' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
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
                let start = i;
                while i < bytes.len() && !matches!(bytes[i], '.' | '#' | '[' | ':') {
                    i += 1;
                }
                let tag: String = bytes[start..i].iter().collect();
                let tag = tag.trim().to_string();
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
fn parse_complex(s: &str) -> Option<Vec<Compound>> {
    let normalized: String = s
        .chars()
        .map(|c| if matches!(c, '>' | '+' | '~') { ' ' } else { c })
        .collect();
    let parts: Vec<Compound> = normalized
        .split_whitespace()
        .filter_map(parse_compound)
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

/// Does `node` match the complex selector `chain`?
fn matches_complex(doc: &dom::Document, node: dom::NodeId, chain: &[Compound]) -> bool {
    if chain.is_empty() {
        return false;
    }
    let last = &chain[chain.len() - 1];
    if !last.matches(doc, node) {
        return false;
    }
    let mut remaining = &chain[..chain.len() - 1];
    let mut cur = doc.get(node).parent;
    while !remaining.is_empty() {
        let want = &remaining[remaining.len() - 1];
        match cur {
            None => return false,
            Some(p) => {
                if want.matches(doc, p) {
                    remaining = &remaining[..remaining.len() - 1];
                }
                cur = doc.get(p).parent;
            }
        }
    }
    true
}

/// Collect every node matching any of the comma-separated selector groups, document order.
fn query_selector_all(doc: &dom::Document, sel: &str) -> Vec<dom::NodeId> {
    let groups: Vec<Vec<Compound>> = sel.split(',').filter_map(parse_complex).collect();
    if groups.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    fn walk(doc: &dom::Document, node: dom::NodeId, groups: &[Vec<Compound>], out: &mut Vec<dom::NodeId>) {
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
fn query_within(doc: &dom::Document, root: dom::NodeId, sel: &str) -> Vec<dom::NodeId> {
    let groups: Vec<Vec<Compound>> = sel.split(',').filter_map(parse_complex).collect();
    let mut out = Vec::new();
    if groups.is_empty() {
        return out;
    }
    fn walk(doc: &dom::Document, node: dom::NodeId, groups: &[Vec<Compound>], out: &mut Vec<dom::NodeId>) {
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
fn collect_by_class(doc: &dom::Document, root: dom::NodeId, wanted: &[String], out: &mut Vec<dom::NodeId>) {
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

/// Render a V8 value to a display string (via JS `String(value)` coercion). Never throws out:
/// uses `to_rust_string_lossy` after a `to_string` coercion, falling back to "undefined".
fn render_value(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> String {
    match value.to_string(scope) {
        Some(s) => s.to_rust_string_lossy(scope),
        None => "undefined".to_string(),
    }
}

/// Read positional argument `i` from a callback as a Rust string (JS-coerced). Missing → "".
fn arg_str(scope: &mut v8::PinScope, args: &v8::FunctionCallbackArguments, i: i32) -> String {
    if i >= args.length() {
        return String::new();
    }
    let v = args.get(i);
    render_value(scope, v)
}

/// Read positional argument `i` as a node id (`usize`). Missing/NaN → None.
fn arg_node(scope: &mut v8::PinScope, args: &v8::FunctionCallbackArguments, i: i32) -> Option<dom::NodeId> {
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
fn js_str<'s>(scope: &mut v8::PinScope<'s, '_>, s: &str) -> v8::Local<'s, v8::Value> {
    match v8::String::new(scope, s) {
        Some(v) => v.into(),
        None => v8::String::empty(scope).into(),
    }
}

/// Build a JS array of node ids (as numbers).
fn js_id_array<'s>(
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
fn js_str_array<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    items: &[String],
) -> v8::Local<'s, v8::Value> {
    let elements: Vec<v8::Local<v8::Value>> =
        items.iter().map(|s| js_str(scope, s)).collect();
    v8::Array::new_with_elements(scope, &elements).into()
}

// ---------------------------------------------------------------------------------------------
// Native primitive callbacks. These are bare functions (V8 callbacks cannot capture state); they
// recover the shared DOM + console from the context slot via `host_state(scope)`. The JS
// bootstrap (DOCUMENT_BOOTSTRAP) builds `document`/element objects on top of these.
// ---------------------------------------------------------------------------------------------

/// `__consoleLog(...args)` — push a space-joined line into the shared console buffer.
fn prim_console_log(
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
fn prim_create_element(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let tag = arg_str(scope, &args, 0);
    let state = host_state(scope);
    let id = state.doc.borrow_mut().alloc(
        dom::NodeData::Element(dom::ElementData { tag, attrs: HashMap::new() }),
        None,
    );
    rv.set_double(id.0 as f64);
}

/// `__createText(text) -> id`
fn prim_create_text(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let text = arg_str(scope, &args, 0);
    let state = host_state(scope);
    let id = state.doc.borrow_mut().alloc(dom::NodeData::Text(text), None);
    rv.set_double(id.0 as f64);
}

/// `__createComment(text) -> id`
fn prim_create_comment(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let text = arg_str(scope, &args, 0);
    let state = host_state(scope);
    let id = state.doc.borrow_mut().alloc(dom::NodeData::Comment(text), None);
    rv.set_double(id.0 as f64);
}

/// `__getAttr(id, name) -> string | null`
fn prim_get_attr(
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

/// `__setAttr(id, name, val)`
fn prim_set_attr(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let name = arg_str(scope, &args, 1);
    let value = arg_str(scope, &args, 2);
    let state = host_state(scope);
    if let Some(n) = node {
        if let dom::NodeData::Element(e) = &mut state.doc.borrow_mut().get_mut(n).data {
            e.attrs.insert(name, value);
        }
    }
}

/// `__removeAttr(id, name)`
fn prim_remove_attr(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let name = arg_str(scope, &args, 1);
    let state = host_state(scope);
    if let Some(n) = node {
        if let dom::NodeData::Element(e) = &mut state.doc.borrow_mut().get_mut(n).data {
            e.attrs.remove(&name);
        }
    }
}

/// `__attrNames(id) -> [name...]`
fn prim_attr_names(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let names: Vec<String> = node
        .map(|n| match &state.doc.borrow().get(n).data {
            dom::NodeData::Element(e) => e.attrs.keys().cloned().collect(),
            _ => Vec::new(),
        })
        .unwrap_or_default();
    let arr = js_str_array(scope, &names);
    rv.set(arr);
}

/// `__appendChild(parentId, childId)` — reparent child under parent.
fn prim_append_child(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let parent = arg_node(scope, &args, 0);
    let child = arg_node(scope, &args, 1);
    let state = host_state(scope);
    if let (Some(parent), Some(child)) = (parent, child) {
        let mut d = state.doc.borrow_mut();
        if let Some(old_parent) = d.get(child).parent {
            d.get_mut(old_parent).children.retain(|&c| c != child);
        }
        d.get_mut(child).parent = Some(parent);
        d.get_mut(parent).children.push(child);
    }
}

/// `__insertBefore(parentId, childId, refIdOrMinus1)`
fn prim_insert_before(
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
        let mut d = state.doc.borrow_mut();
        if let Some(old) = d.get(child).parent {
            d.get_mut(old).children.retain(|&c| c != child);
        }
        d.get_mut(child).parent = Some(parent);
        let pos = ref_node.and_then(|r| d.get(parent).children.iter().position(|&c| c == r));
        match pos {
            Some(i) => d.get_mut(parent).children.insert(i, child),
            None => d.get_mut(parent).children.push(child),
        }
    }
}

/// `__removeChild(parentId, childId)`
fn prim_remove_child(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let parent = arg_node(scope, &args, 0);
    let child = arg_node(scope, &args, 1);
    let state = host_state(scope);
    if let (Some(parent), Some(child)) = (parent, child) {
        let mut d = state.doc.borrow_mut();
        d.get_mut(parent).children.retain(|&c| c != child);
        if d.get(child).parent == Some(parent) {
            d.get_mut(child).parent = None;
        }
    }
}

/// `__children(id) -> [id...]` (all child nodes, in order)
fn prim_children(
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
fn prim_parent(
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
fn prim_tag(
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

/// `__nodeType(id) -> 1 | 3 | 8 | 9`
fn prim_node_type(
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
            dom::NodeData::Comment(_) => 8,
            dom::NodeData::Document => 9,
        })
        .unwrap_or(1);
    rv.set_int32(ty);
}

/// `__textContent(id) -> string`
fn prim_text_content(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let s = node.map(|n| text_content(&state.doc.borrow(), n)).unwrap_or_default();
    let v = js_str(scope, &s);
    rv.set(v);
}

/// `__setTextContent(id, text)`
fn prim_set_text_content(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let text = arg_str(scope, &args, 1);
    let state = host_state(scope);
    if let Some(n) = node {
        set_text_content(&mut state.doc.borrow_mut(), n, &text);
    }
}

/// `__innerHTML(id) -> string`
fn prim_inner_html(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let s = node.map(|n| inner_html(&state.doc.borrow(), n)).unwrap_or_default();
    let v = js_str(scope, &s);
    rv.set(v);
}

/// `__setInnerHTML(id, html)`
fn prim_set_inner_html(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let html = arg_str(scope, &args, 1);
    let state = host_state(scope);
    if let Some(n) = node {
        set_inner_html(&mut state.doc.borrow_mut(), n, &html);
    }
}

/// `__getElementById(idStr) -> id | -1`
fn prim_get_element_by_id(
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
fn prim_query_selector_all(
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
fn prim_query_selector_all_within(
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
fn prim_get_elements_by_tag_name(
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
fn prim_get_elements_by_tag_name_within(
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
fn prim_get_elements_by_class_name(
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
fn prim_document_element_id(
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

/// `__bodyId() -> id | -1`
fn prim_body_id(
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
fn prim_head_id(
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
fn prim_root_id(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let state = host_state(scope);
    let root = state.doc.borrow().root();
    rv.set_double(root.0 as f64);
}

/// `__titleText() -> string`
fn prim_title_text(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let state = host_state(scope);
    let s = {
        let d = state.doc.borrow();
        find_by_tag(&d, d.root(), "title").map(|n| text_content(&d, n)).unwrap_or_default()
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
fn prim_fetch(
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
    let body = (host_state(scope).fetcher)(&resolved);
    match body {
        Some(s) => {
            let v = js_str(scope, &s);
            rv.set(v);
        }
        None => rv.set_null(),
    }
}

// ---------------------------------------------------------------------------------------------
// Installation: register native primitives + evaluate the JS bootstrap onto a fresh context.
// ---------------------------------------------------------------------------------------------

/// Define a native function on `target` under `name`.
fn set_fn(
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
fn install_console_sink(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
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

/// Install the node-id DOM primitives onto `globalThis`.
fn install_dom_primitives(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    set_fn(scope, global, "__createElement", prim_create_element);
    set_fn(scope, global, "__createText", prim_create_text);
    set_fn(scope, global, "__createComment", prim_create_comment);
    set_fn(scope, global, "__getAttr", prim_get_attr);
    set_fn(scope, global, "__setAttr", prim_set_attr);
    set_fn(scope, global, "__removeAttr", prim_remove_attr);
    set_fn(scope, global, "__attrNames", prim_attr_names);
    set_fn(scope, global, "__appendChild", prim_append_child);
    set_fn(scope, global, "__insertBefore", prim_insert_before);
    set_fn(scope, global, "__removeChild", prim_remove_child);
    set_fn(scope, global, "__children", prim_children);
    set_fn(scope, global, "__parent", prim_parent);
    set_fn(scope, global, "__tag", prim_tag);
    set_fn(scope, global, "__nodeType", prim_node_type);
    set_fn(scope, global, "__textContent", prim_text_content);
    set_fn(scope, global, "__setTextContent", prim_set_text_content);
    set_fn(scope, global, "__innerHTML", prim_inner_html);
    set_fn(scope, global, "__setInnerHTML", prim_set_inner_html);
    set_fn(scope, global, "__getElementById", prim_get_element_by_id);
    set_fn(scope, global, "__querySelectorAll", prim_query_selector_all);
    set_fn(scope, global, "__querySelectorAllWithin", prim_query_selector_all_within);
    set_fn(scope, global, "__getElementsByTagName", prim_get_elements_by_tag_name);
    set_fn(scope, global, "__getElementsByTagNameWithin", prim_get_elements_by_tag_name_within);
    set_fn(scope, global, "__getElementsByClassName", prim_get_elements_by_class_name);
    set_fn(scope, global, "__documentElementId", prim_document_element_id);
    set_fn(scope, global, "__bodyId", prim_body_id);
    set_fn(scope, global, "__headId", prim_head_id);
    set_fn(scope, global, "__rootId", prim_root_id);
    set_fn(scope, global, "__titleText", prim_title_text);
    set_fn(scope, global, "__fetch", prim_fetch);
}

/// Compile+run a script in the current context, ignoring the result. Used for bootstraps where a
/// failure would be a build-time bug (we surface it via a panic in debug-style assertions).
fn eval_internal(scope: &mut v8::PinScope, source: &str, name: &str) -> bool {
    v8::tc_scope!(let tc, scope);
    let code = match v8::String::new(tc, source) {
        Some(c) => c,
        None => return false,
    };
    let resource = v8::String::new(tc, name).unwrap();
    let origin = v8::ScriptOrigin::new(
        tc,
        resource.into(),
        0,
        0,
        false,
        0,
        None,
        false,
        false,
        false,
        None,
    );
    let script = match v8::Script::compile(tc, code, Some(&origin)) {
        Some(s) => s,
        None => return false,
    };
    script.run(tc).is_some()
}

/// Install the full DOM-aware browser environment into the current context: console, the DOM
/// primitives + JS `document`/element layer, the timer/event loop, and the navigator/location/etc.
/// bootstrap. `__pageURL` is set as a real string value (no source interpolation) before the
/// browser-env bootstrap reads it.
/// Live display metrics (logical viewport size + backing scale), set by the engine via
/// [`set_device_metrics`] so JS sees the real `window.innerWidth/innerHeight` and
/// `devicePixelRatio` instead of hardcoded defaults. Stored as atomics so the engine (any thread)
/// can update them and the JS worker reads them when building the environment.
static VP_W: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1200);
static VP_H: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(780);
static DPR_BITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Set the logical viewport size (px) and device pixel ratio surfaced to page JS.
pub fn set_device_metrics(width: u32, height: u32, device_pixel_ratio: f32) {
    use std::sync::atomic::Ordering;
    VP_W.store(width.max(1), Ordering::Relaxed);
    VP_H.store(height.max(1), Ordering::Relaxed);
    DPR_BITS.store(device_pixel_ratio.max(0.1).to_bits(), Ordering::Relaxed);
}

fn device_metrics() -> (f64, f64, f64) {
    use std::sync::atomic::Ordering;
    let bits = DPR_BITS.load(Ordering::Relaxed);
    let dpr = if bits == 0 { 2.0 } else { f32::from_bits(bits) };
    (
        VP_W.load(Ordering::Relaxed) as f64,
        VP_H.load(Ordering::Relaxed) as f64,
        dpr as f64,
    )
}

fn install_browser_environment(scope: &mut v8::PinScope, url: &str) {
    let global = scope.get_current_context().global(scope);
    install_console_sink(scope, global);
    install_dom_primitives(scope, global);
    // Build `window`/`self`/`globalThis` aliases + the JS `document` over the primitives.
    eval_internal(scope, DOCUMENT_BOOTSTRAP, "<document>");
    // Timers / event loop.
    eval_internal(scope, TIMERS_BOOTSTRAP, "<timers>");
    // Set the page URL as a real string value, then run the browser-env bootstrap.
    let key = v8::String::new(scope, "__pageURL").unwrap();
    let val = js_str(scope, url);
    global.set(scope, key.into(), val);
    // Inject the live viewport metrics so the bootstrap can set window.innerWidth/innerHeight and
    // devicePixelRatio from the real values rather than hardcoded defaults.
    let (vw, vh, dpr) = device_metrics();
    for (name, num) in [("__innerWidth", vw), ("__innerHeight", vh), ("__devicePixelRatio", dpr)] {
        let k = v8::String::new(scope, name).unwrap();
        let n = v8::Number::new(scope, num);
        global.set(scope, k.into(), n.into());
    }
    eval_internal(scope, BROWSER_ENV_BOOTSTRAP, "<browser-env>");
}

/// JS bootstrap that builds `window`/`self`/`globalThis` aliases and the `document` object +
/// element wrapper layer on top of the node-id native primitives (`__createElement`, `__getAttr`,
/// `__appendChild`, ...). This replaces the old Rust-side per-node wrapper closures: every element
/// is a plain JS object carrying a hidden `__node` id, with accessors/methods that call the
/// primitives. The browser-env bootstrap's `canon`/`enrichElement` machinery then layers wrapper
/// caching, `style`/`classList`/`dataset` write-through, and the DOM interface prototype chain on
/// top — exactly as before — because these wrappers expose the same shape the old native layer did
/// (fresh object carrying `__node`, with the same method/accessor names).
const DOCUMENT_BOOTSTRAP: &str = r##"
(function () {
  function def(obj, name, value) {
    Object.defineProperty(obj, name, { value: value, enumerable: false, configurable: true, writable: true });
  }

  // window / self aliases (globalThis already exists).
  globalThis.window = globalThis;
  globalThis.self = globalThis;
  // Minimal location stub (overwritten by the browser-env bootstrap).
  globalThis.location = { href: "" };

  var NODE = "__node";

  // Build a fresh element wrapper object for a node id. Carries `__node` plus accessors/methods
  // that delegate to the native primitives. Returns null for id === -1.
  function wrap(id) {
    if (typeof id !== "number" || id < 0) { return null; }
    var el = {};
    def(el, NODE, id);

    function uc(s) { return String(s == null ? "" : s).toUpperCase(); }

    Object.defineProperty(el, "tagName", { get: function () { return uc(__tag(id)); }, enumerable: true, configurable: true });
    Object.defineProperty(el, "nodeName", { get: function () {
      var t = __nodeType(id);
      if (t === 3) { return "#text"; }
      if (t === 8) { return "#comment"; }
      if (t === 9) { return "#document"; }
      return uc(__tag(id));
    }, enumerable: true, configurable: true });
    Object.defineProperty(el, "nodeType", { get: function () { return __nodeType(id); }, enumerable: true, configurable: true });

    Object.defineProperty(el, "textContent", {
      get: function () { return __textContent(id); },
      set: function (v) { __setTextContent(id, v == null ? "" : String(v)); },
      enumerable: true, configurable: true
    });
    // `data` / `nodeValue` mirror textContent — used by Vue when patching text/comment anchors.
    Object.defineProperty(el, "data", {
      get: function () { return __textContent(id); },
      set: function (v) { __setTextContent(id, v == null ? "" : String(v)); },
      enumerable: true, configurable: true
    });
    Object.defineProperty(el, "nodeValue", {
      get: function () { var t = __nodeType(id); return (t === 3 || t === 8) ? __textContent(id) : null; },
      set: function (v) { __setTextContent(id, v == null ? "" : String(v)); },
      enumerable: true, configurable: true
    });
    Object.defineProperty(el, "innerHTML", {
      get: function () { return __innerHTML(id); },
      set: function (v) { __setInnerHTML(id, v == null ? "" : String(v)); },
      enumerable: true, configurable: true
    });
    Object.defineProperty(el, "outerHTML", {
      get: function () { try { return __innerHTML(__parent(id) >= 0 ? id : id); } catch (e) { return ""; } },
      enumerable: true, configurable: true
    });

    Object.defineProperty(el, "id", {
      get: function () { var v = __getAttr(id, "id"); return v == null ? "" : v; },
      set: function (v) { __setAttr(id, "id", v == null ? "" : String(v)); },
      enumerable: true, configurable: true
    });
    Object.defineProperty(el, "className", {
      get: function () { var v = __getAttr(id, "class"); return v == null ? "" : v; },
      set: function (v) { __setAttr(id, "class", v == null ? "" : String(v)); },
      enumerable: true, configurable: true
    });

    def(el, "getAttribute", function (name) { return __getAttr(id, String(name)); });
    def(el, "setAttribute", function (name, value) { __setAttr(id, String(name), value == null ? "" : String(value)); });
    def(el, "removeAttribute", function (name) { __removeAttr(id, String(name)); });
    def(el, "hasAttribute", function (name) { return __getAttr(id, String(name)) != null; });
    def(el, "getAttributeNames", function () { return __attrNames(id); });

    def(el, "appendChild", function (child) {
      if (child && typeof child.__node === "number") { __appendChild(id, child.__node); }
      return child;
    });
    def(el, "removeChild", function (child) {
      if (child && typeof child.__node === "number") { __removeChild(id, child.__node); }
      return child;
    });
    def(el, "insertBefore", function (newNode, refNode) {
      if (newNode && typeof newNode.__node === "number") {
        var refId = (refNode && typeof refNode.__node === "number") ? refNode.__node : -1;
        __insertBefore(id, newNode.__node, refId);
      }
      return newNode;
    });
    def(el, "replaceChild", function (newNode, oldNode) {
      if (newNode && typeof newNode.__node === "number" && oldNode && typeof oldNode.__node === "number") {
        __insertBefore(id, newNode.__node, oldNode.__node);
        __removeChild(id, oldNode.__node);
      }
      return oldNode;
    });
    def(el, "remove", function () { var p = __parent(id); if (p >= 0) { __removeChild(p, id); } });
    def(el, "append", function () {
      for (var i = 0; i < arguments.length; i++) { var c = arguments[i]; if (c && typeof c.__node === "number") { __appendChild(id, c.__node); } }
    });
    def(el, "prepend", function () {
      var kids = __children(id); var first = kids.length ? kids[0] : -1;
      for (var i = 0; i < arguments.length; i++) { var c = arguments[i]; if (c && typeof c.__node === "number") { __insertBefore(id, c.__node, first); } }
    });

    def(el, "insertAdjacentElement", function (position, node) {
      var pos = String(position == null ? "" : position).toLowerCase();
      if (!node || typeof node.__node !== "number") { return null; }
      var nid = node.__node;
      var p;
      if (pos === "beforebegin") { p = __parent(id); if (p >= 0) { __insertBefore(p, nid, id); } }
      else if (pos === "afterbegin") { var k = __children(id); __insertBefore(id, nid, k.length ? k[0] : -1); }
      else if (pos === "beforeend") { __appendChild(id, nid); }
      else if (pos === "afterend") { p = __parent(id); if (p >= 0) { var sibs = __children(p); var idx = sibs.indexOf(id); var ref = (idx >= 0 && idx + 1 < sibs.length) ? sibs[idx + 1] : -1; __insertBefore(p, nid, ref); } }
      else { throw new SyntaxError("Failed to execute 'insertAdjacentElement': '" + position + "' is not a valid value."); }
      return node;
    });

    def(el, "insertAdjacentHTML", function (position, html) {
      var pos = String(position == null ? "" : position).toLowerCase();
      if (pos !== "beforebegin" && pos !== "afterbegin" && pos !== "beforeend" && pos !== "afterend") {
        throw new SyntaxError("Failed to execute 'insertAdjacentHTML': '" + position + "' is not a valid value.");
      }
      // Parse the HTML fragment into real nodes via a temp container, then move them.
      var tmp = __createElement("template");
      __setInnerHTML(tmp, html == null ? "" : String(html));
      var parsed = __children(tmp).slice();
      if (pos === "beforebegin") {
        var p = __parent(id); if (p < 0) { return; }
        for (var i = 0; i < parsed.length; i++) { __insertBefore(p, parsed[i], id); }
      } else if (pos === "afterbegin") {
        var k = __children(id); var ref = k.length ? k[0] : -1;
        for (var i = 0; i < parsed.length; i++) { __insertBefore(id, parsed[i], ref); }
      } else if (pos === "beforeend") {
        for (var i = 0; i < parsed.length; i++) { __appendChild(id, parsed[i]); }
      } else { // afterend
        var p2 = __parent(id); if (p2 < 0) { return; }
        var sibs = __children(p2); var idx = sibs.indexOf(id);
        var ref2 = (idx >= 0 && idx + 1 < sibs.length) ? sibs[idx + 1] : -1;
        for (var i = 0; i < parsed.length; i++) { __insertBefore(p2, parsed[i], ref2); }
      }
    });

    def(el, "insertAdjacentText", function (position, text) {
      var t = document.createTextNode(text == null ? "" : String(text));
      return el.insertAdjacentElement(position, t);
    });

    def(el, "contains", function (other) {
      if (!other || typeof other.__node !== "number") { return false; }
      var cur = other.__node;
      while (cur >= 0) { if (cur === id) { return true; } cur = __parent(cur); }
      return false;
    });

    def(el, "querySelector", function (sel) { var r = __querySelectorAllWithin(id, String(sel)); return r.length ? wrap(r[0]) : null; });
    def(el, "querySelectorAll", function (sel) { return __querySelectorAllWithin(id, String(sel)).map(wrap); });
    def(el, "getElementsByTagName", function (tag) { return __getElementsByTagNameWithin(id, String(tag)).map(wrap); });
    def(el, "getElementsByClassName", function (cls) {
      // Scope getElementsByClassName by filtering the global result to descendants of `id`.
      var wanted = String(cls).split(/\s+/).filter(Boolean);
      var all = __getElementsByClassName(String(cls));
      var out = [];
      for (var i = 0; i < all.length; i++) {
        var cur = __parent(all[i]); var isDesc = false;
        while (cur >= 0) { if (cur === id) { isDesc = true; break; } cur = __parent(cur); }
        if (isDesc) { out.push(wrap(all[i])); }
      }
      return out;
    });

    def(el, "matches", function (sel) {
      // An element matches `sel` if it appears in the document-wide result set.
      var r = __querySelectorAll(String(sel));
      for (var i = 0; i < r.length; i++) { if (r[i] === id) { return true; } }
      return false;
    });
    def(el, "closest", function (sel) {
      var cur = id;
      while (cur >= 0) {
        var w = wrap(cur);
        if (w && w.matches(sel)) { return w; }
        cur = __parent(cur);
      }
      return null;
    });

    // Navigation accessors (return fresh wrappers; the enrich layer canonicalizes them).
    function childList(elementsOnly) {
      var kids = __children(id); var out = [];
      for (var i = 0; i < kids.length; i++) {
        if (!elementsOnly || __nodeType(kids[i]) === 1) { out.push(wrap(kids[i])); }
      }
      return out;
    }
    Object.defineProperty(el, "children", { get: function () { return childList(true); }, enumerable: true, configurable: true });
    Object.defineProperty(el, "childNodes", { get: function () { return childList(false); }, enumerable: true, configurable: true });
    Object.defineProperty(el, "parentNode", { get: function () { return wrap(__parent(id)); }, enumerable: true, configurable: true });
    Object.defineProperty(el, "parentElement", { get: function () { var p = __parent(id); return (p >= 0 && __nodeType(p) === 1) ? wrap(p) : (p >= 0 ? wrap(p) : null); }, enumerable: true, configurable: true });
    Object.defineProperty(el, "firstChild", { get: function () { var k = __children(id); return k.length ? wrap(k[0]) : null; }, enumerable: true, configurable: true });
    Object.defineProperty(el, "lastChild", { get: function () { var k = __children(id); return k.length ? wrap(k[k.length - 1]) : null; }, enumerable: true, configurable: true });
    Object.defineProperty(el, "firstElementChild", { get: function () { var c = childList(true); return c.length ? c[0] : null; }, enumerable: true, configurable: true });
    Object.defineProperty(el, "lastElementChild", { get: function () { var c = childList(true); return c.length ? c[c.length - 1] : null; }, enumerable: true, configurable: true });

    function sibling(next, elementOnly) {
      var p = __parent(id); if (p < 0) { return null; }
      var sibs = __children(p);
      var idx = sibs.indexOf(id); if (idx < 0) { return null; }
      var i = idx;
      while (true) {
        if (next) { i++; if (i >= sibs.length) { return null; } }
        else { i--; if (i < 0) { return null; } }
        if (!elementOnly || __nodeType(sibs[i]) === 1) { return wrap(sibs[i]); }
      }
    }
    Object.defineProperty(el, "nextSibling", { get: function () { return sibling(true, false); }, enumerable: true, configurable: true });
    Object.defineProperty(el, "previousSibling", { get: function () { return sibling(false, false); }, enumerable: true, configurable: true });
    Object.defineProperty(el, "nextElementSibling", { get: function () { return sibling(true, true); }, enumerable: true, configurable: true });
    Object.defineProperty(el, "previousElementSibling", { get: function () { return sibling(false, true); }, enumerable: true, configurable: true });

    return el;
  }
  def(globalThis, "__wrapNode", wrap);

  // --- document --------------------------------------------------------------------------------
  var document = {};
  def(document, "getElementById", function (idStr) { var n = __getElementById(String(idStr)); return n >= 0 ? wrap(n) : null; });
  def(document, "getElementsByTagName", function (tag) { return __getElementsByTagName(String(tag)).map(wrap); });
  def(document, "getElementsByClassName", function (cls) { return __getElementsByClassName(String(cls)).map(wrap); });
  def(document, "querySelector", function (sel) { var r = __querySelectorAll(String(sel)); return r.length ? wrap(r[0]) : null; });
  def(document, "querySelectorAll", function (sel) { return __querySelectorAll(String(sel)).map(wrap); });
  def(document, "createElement", function (tag) { return wrap(__createElement(String(tag))); });
  // Node-id-keyed attribute helpers the browser-env bootstrap uses for style/classList/dataset.
  def(document, "__getAttr", function (node, name) { return __getAttr(node, String(name)); });
  def(document, "__setAttr", function (node, name, value) { __setAttr(node, String(name), value == null ? "" : String(value)); });
  def(document, "__removeAttr", function (node, name) { __removeAttr(node, String(name)); });

  Object.defineProperty(document, "title", {
    get: function () { return __titleText(); },
    set: function (v) {
      var head = __headId();
      var t = -1;
      var all = __getElementsByTagName("title");
      if (all.length) { t = all[0]; }
      if (t < 0) {
        t = __createElement("title");
        var parent = head >= 0 ? head : __documentElementId();
        if (parent >= 0) { __appendChild(parent, t); }
      }
      if (t >= 0) { __setTextContent(t, v == null ? "" : String(v)); }
    },
    enumerable: true, configurable: true
  });
  Object.defineProperty(document, "body", { get: function () { var n = __bodyId(); return n >= 0 ? wrap(n) : null; }, enumerable: true, configurable: true });
  Object.defineProperty(document, "documentElement", { get: function () { var n = __documentElementId(); return n >= 0 ? wrap(n) : null; }, enumerable: true, configurable: true });
  Object.defineProperty(document, "head", { get: function () { var n = __headId(); return n >= 0 ? wrap(n) : null; }, enumerable: true, configurable: true });
  def(document, "nodeType", 9);

  globalThis.document = document;
})();
"##;

/// JS bootstrap implementing the timer / event-loop APIs. Engine-agnostic — reused verbatim.
/// All scheduling lives here; Rust only drives via `__runDueTimers()` and reads `__timerErrors`.
const TIMERS_BOOTSTRAP: &str = r#"
(function () {
  // Two-phase clock: VIRTUAL at load (fast-forward to fire pending one-shots so the first paint is
  // complete; intervals may spin, bounded by the cap), and the REAL wall clock once the page is
  // live (driven by Engine::tick) so setInterval/setTimeout/rAF fire on actual elapsed time.
  var loop = { timers: [], micro: [], nextId: 1, now: 0, realBase: 0, realtime: false, firedThisDrain: Object.create(null) };
  Object.defineProperty(globalThis, "__eventLoop", { value: loop, enumerable: false, configurable: true, writable: true });
  Object.defineProperty(globalThis, "__timerErrors", { value: [], enumerable: false, configurable: true, writable: true });
  function nowMs() { try { return Date.now(); } catch (e) { return 0; } }
  function currentTime() { return loop.realtime ? (loop.now + (nowMs() - loop.realBase)) : loop.now; }

  function schedule(fn, delay, args, repeat) {
    if (typeof fn !== "function") { return 0; }
    var d = Number(delay) || 0;
    if (d < 0 || d !== d) { d = 0; }
    var id = loop.nextId++;
    loop.timers.push({ id: id, fn: fn, delay: d, args: args, when: currentTime() + d, repeat: repeat });
    return id;
  }

  function define(name, fn) {
    Object.defineProperty(globalThis, name, { value: fn, enumerable: false, configurable: true, writable: true });
  }

  define("setTimeout", function (fn, delay) {
    var args = Array.prototype.slice.call(arguments, 2);
    return schedule(fn, delay, args, false);
  });
  define("setInterval", function (fn, delay) {
    var args = Array.prototype.slice.call(arguments, 2);
    return schedule(fn, delay, args, true);
  });
  define("clearTimeout", function (id) {
    if (id == null) { return; }
    for (var i = 0; i < loop.timers.length; i++) {
      if (loop.timers[i].id === id) { loop.timers.splice(i, 1); return; }
    }
  });
  define("clearInterval", globalThis.clearTimeout);

  define("queueMicrotask", function (fn) {
    if (typeof fn !== "function") { throw new TypeError("queueMicrotask: argument is not a function"); }
    loop.micro.push(fn);
  });

  define("requestAnimationFrame", function (fn) {
    // No real frames; schedule ~16ms out (one 60fps frame) so rAF runs after 0ms timers.
    return schedule(fn, 16, [currentTime() + 16], false);
  });
  define("cancelAnimationFrame", globalThis.clearTimeout);

  // Reset the per-drain "already fired" set (Rust calls at each drain start) so an interval can't
  // spin within a single realtime tick.
  define("__beginDrain", function () { loop.firedThisDrain = Object.create(null); });
  // Switch from the load-time virtual clock to the real wall clock (Rust calls once the page is
  // live); re-arm surviving repeating timers to fire `delay` ms from now (real time).
  define("__enterRealtime", function () {
    if (loop.realtime) { return; }
    loop.realtime = true;
    loop.realBase = nowMs();
    for (var i = 0; i < loop.timers.length; i++) { if (loop.timers[i].repeat) { loop.timers[i].when = loop.now + loop.timers[i].delay; } }
  });

  // Driver called from Rust. Returns true if it ran a task (microtask or timer), false if nothing
  // is currently runnable. One throwing task does not kill the loop: errors are collected.
  define("__runDueTimers", function () {
    // 1. Drain ALL microtasks first (FIFO), including ones queued while draining.
    var ranSomething = false;
    while (loop.micro.length > 0) {
      var m = loop.micro.shift();
      ranSomething = true;
      try { m(); } catch (e) { globalThis.__timerErrors.push(String(e)); }
    }
    if (ranSomething) { return true; }

    // 2. Pick the smallest-`when` timer (skipping a repeat already fired this realtime tick).
    if (loop.timers.length === 0) { return false; }
    // A repeating timer fires at most once per drain (load OR tick) so an interval can't spin to
    // the cap — its callback runs once at load, then once per real-time tick thereafter.
    var bestIdx = -1, best = null;
    for (var i = 0; i < loop.timers.length; i++) {
      var t = loop.timers[i];
      if (t.repeat && loop.firedThisDrain[t.id]) { continue; }
      if (bestIdx < 0 || t.when < best.when || (t.when === best.when && t.id < best.id)) { bestIdx = i; best = t; }
    }
    if (bestIdx < 0) { return false; }
    var timer = loop.timers[bestIdx];
    if (loop.realtime) {
      // Real clock: fire only once the scheduled instant has actually elapsed.
      if (timer.when > currentTime()) { return false; }
      if (timer.repeat) { timer.when = timer.when + timer.delay; loop.firedThisDrain[timer.id] = true; }
      else { loop.timers.splice(bestIdx, 1); }
    } else {
      // Load-time: fast-forward virtual time to this timer and fire it (one-shots and rAF chains
      // run freely; a repeating timer fires once and is parked for the real-time ticks).
      if (timer.when > loop.now) { loop.now = timer.when; }
      if (timer.repeat) { timer.when = loop.now + timer.delay; loop.firedThisDrain[timer.id] = true; }
      else { loop.timers.splice(bestIdx, 1); }
    }
    try { timer.fn.apply(undefined, timer.args); }
    catch (e) { globalThis.__timerErrors.push(String(e)); }
    return true;
  });
})();
"#;

/// JS bootstrap implementing the standard browser environment (navigator/location/history/
/// storage/screen/matchMedia/getComputedStyle/event model/observers/URL/etc.) plus the per-node
/// wrapper cache, `style`/`classList`/`dataset` write-through, and the DOM interface class
/// hierarchy. Engine-agnostic — reused verbatim from the prior implementation; it talks to the
/// document via the JS `document` layer + the node-id `document.__getAttr/__setAttr/__removeAttr`
/// helpers (now built over the native primitives in DOCUMENT_BOOTSTRAP).
const BROWSER_ENV_BOOTSTRAP: &str = r#"
(function () {
  function def(obj, name, value) {
    Object.defineProperty(obj, name, { value: value, enumerable: false, configurable: true, writable: true });
  }
  function fn() {}

  // --- legacy / missing language polyfills the host engine lacks ---------------------------
  // String.prototype.substr (deprecated but heavily used by real-world minified code, e.g.
  // google's URL-encoding helpers). Without it `"x".substr(1)` throws "not a callable function".
  if (typeof String.prototype.substr !== "function") {
    def(String.prototype, "substr", function (start, length) {
      var s = String(this);
      var len = s.length;
      start = start === undefined ? 0 : (start | 0);
      if (start < 0) { start = Math.max(len + start, 0); }
      var count = length === undefined ? (len - start) : (length | 0);
      if (count <= 0 || start >= len) { return ""; }
      count = Math.min(count, len - start);
      return s.slice(start, start + count);
    });
  }

  // --- navigator (plain object so enumeration / Object.keys / Object.assign work) ----------
  var ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Safari/605.1.15";
  globalThis.navigator = {
    userAgent: ua,
    appVersion: "5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Safari/605.1.15",
    appName: "Netscape",
    appCodeName: "Mozilla",
    product: "Gecko",
    platform: "MacIntel",
    vendor: "Apple Computer, Inc.",
    vendorSub: "",
    language: "en-US",
    languages: ["en-US", "en"],
    onLine: true,
    cookieEnabled: true,
    doNotTrack: null,
    maxTouchPoints: 0,
    hardwareConcurrency: 8,
    deviceMemory: 8,
    webdriver: false,
    plugins: [],
    mimeTypes: [],
    userActivation: { hasBeenActive: false, isActive: false },
    sendBeacon: function () { return false; },
    clipboard: {},
    geolocation: {
      getCurrentPosition: function () {},
      watchPosition: function () { return 0; },
      clearWatch: function () {}
    }
  };

  // --- location (populated from globalThis.__pageURL below) --------------------------------
  function parseURL(url) {
    url = String(url == null ? "" : url);
    // scheme://host/path?query#hash  (host = userinfo@hostname:port)
    var m = /^([a-zA-Z][a-zA-Z0-9+.\-]*:)?(?:\/\/([^\/?#]*))?([^?#]*)(\?[^#]*)?(#.*)?$/.exec(url) || [];
    var protocol = m[1] || "";
    var authority = m[2] || "";
    var pathname = m[3] || "";
    var search = m[4] || "";
    var hash = m[5] || "";
    // Strip any userinfo, then split host into hostname:port.
    var at = authority.lastIndexOf("@");
    var host = at >= 0 ? authority.slice(at + 1) : authority;
    var hostname = host, port = "";
    var colon = host.lastIndexOf(":");
    if (colon >= 0 && host.indexOf("]", colon) < 0) { hostname = host.slice(0, colon); port = host.slice(colon + 1); }
    if (pathname === "" && protocol && host) { pathname = "/"; }
    var origin = (protocol && host) ? (protocol + "//" + host) : "null";
    return {
      href: url, protocol: protocol, host: host, hostname: hostname, port: port,
      pathname: pathname, search: search, hash: hash, origin: origin
    };
  }

  var parts = parseURL(globalThis.__pageURL);
  var location = {
    href: parts.href, protocol: parts.protocol, host: parts.host, hostname: parts.hostname,
    port: parts.port, pathname: parts.pathname, search: parts.search, hash: parts.hash,
    origin: parts.origin,
    assign: fn, replace: fn, reload: fn,
    toString: function () { return this.href; }
  };
  // `location` already exists (a minimal stub from install_globals); overwrite it.
  globalThis.location = location;

  // --- history -----------------------------------------------------------------------------
  globalThis.history = {
    length: 1, scrollRestoration: "auto", state: null,
    pushState: fn, replaceState: fn, back: fn, forward: fn, go: fn
  };

  // --- Storage (localStorage / sessionStorage) ---------------------------------------------
  function makeStorage() {
    var map = Object.create(null);
    var s = {
      getItem: function (k) { k = String(k); return Object.prototype.hasOwnProperty.call(map, k) ? map[k] : null; },
      setItem: function (k, v) { map[String(k)] = String(v); },
      removeItem: function (k) { delete map[String(k)]; },
      clear: function () { map = Object.create(null); },
      key: function (i) { var ks = Object.keys(map); return i >= 0 && i < ks.length ? ks[i] : null; }
    };
    Object.defineProperty(s, "length", { get: function () { return Object.keys(map).length; }, enumerable: false, configurable: true });
    return s;
  }
  globalThis.localStorage = makeStorage();
  globalThis.sessionStorage = makeStorage();

  // --- screen ------------------------------------------------------------------------------
  globalThis.screen = {
    width: 1512, height: 982, availWidth: 1512, availHeight: 944,
    colorDepth: 24, pixelDepth: 24,
    orientation: { type: "landscape-primary", angle: 0 }
  };

  // --- window metrics + no-op window methods -----------------------------------------------
  // Real viewport + scale injected by the engine (fall back to defaults if absent).
  var __iw = (typeof globalThis.__innerWidth === "number" && globalThis.__innerWidth > 0) ? globalThis.__innerWidth : 1200;
  var __ih = (typeof globalThis.__innerHeight === "number" && globalThis.__innerHeight > 0) ? globalThis.__innerHeight : 780;
  globalThis.innerWidth = __iw; globalThis.innerHeight = __ih;
  globalThis.outerWidth = __iw; globalThis.outerHeight = __ih + 40;
  globalThis.devicePixelRatio = (typeof globalThis.__devicePixelRatio === "number" && globalThis.__devicePixelRatio > 0) ? globalThis.__devicePixelRatio : 2;
  globalThis.scrollX = 0; globalThis.scrollY = 0;
  globalThis.pageXOffset = 0; globalThis.pageYOffset = 0;
  globalThis.screenX = 0; globalThis.screenY = 0; globalThis.screenLeft = 0; globalThis.screenTop = 0;
  globalThis.scrollTo = fn; globalThis.scrollBy = fn; globalThis.scroll = fn;
  globalThis.moveTo = fn; globalThis.moveBy = fn; globalThis.resizeTo = fn; globalThis.resizeBy = fn;
  globalThis.focus = fn; globalThis.blur = fn; globalThis.print = fn;
  globalThis.open = function () { return null; }; globalThis.close = fn; globalThis.stop = fn;
  globalThis.getSelection = function () { return null; };
  globalThis.alert = fn; globalThis.confirm = function () { return false; }; globalThis.prompt = function () { return null; };

  // --- matchMedia --------------------------------------------------------------------------
  globalThis.matchMedia = function (q) {
    return {
      matches: false, media: String(q), onchange: null,
      addListener: fn, removeListener: fn,
      addEventListener: fn, removeEventListener: fn,
      dispatchEvent: function () { return false; }
    };
  };

  // --- getComputedStyle --------------------------------------------------------------------
  // Proxy so any property access returns "" and getPropertyValue() returns "". Falls back to a
  // plain object with common props if Proxy is unavailable.
  globalThis.getComputedStyle = function () {
    var base = { getPropertyValue: function () { return ""; }, getPropertyPriority: function () { return ""; }, setProperty: fn, removeProperty: function () { return ""; }, item: function () { return ""; }, length: 0 };
    try {
      return new Proxy(base, {
        get: function (target, prop) {
          if (prop in target) { return target[prop]; }
          return "";
        }
      });
    } catch (e) {
      var common = ["display", "color", "width", "height", "visibility", "opacity", "position", "margin", "padding", "font-size", "background-color"];
      for (var i = 0; i < common.length; i++) { base[common[i]] = ""; }
      return base;
    }
  };

  // --- event model (no-op but present) + a simple listener registry ------------------------
  function installEvents(target) {
    if (!target || typeof target !== "object") { return; }
    if (target.__listeners) { return; } // already installed
    var registry = Object.create(null);
    def(target, "__listeners", registry);
    def(target, "addEventListener", function (type, cb) {
      if (typeof cb !== "function") { return; }
      type = String(type);
      (registry[type] || (registry[type] = [])).push(cb);
    });
    def(target, "removeEventListener", function (type, cb) {
      type = String(type);
      var list = registry[type];
      if (!list) { return; }
      for (var i = 0; i < list.length; i++) { if (list[i] === cb) { list.splice(i, 1); return; } }
    });
    def(target, "dispatchEvent", function (ev) {
      var type = ev && ev.type ? String(ev.type) : "";
      var list = registry[type];
      if (list) {
        var copy = list.slice();
        for (var i = 0; i < copy.length; i++) {
          try { copy[i].call(target, ev); } catch (e) { (globalThis.__timerErrors || []).push(String(e)); }
        }
      }
      return true;
    });
  }
  installEvents(globalThis);
  installEvents(document);

  // --- DOM lifecycle dispatch (driven from Rust during the drain) --------------------------
  var readyState = "loading";
  Object.defineProperty(document, "readyState", { get: function () { return readyState; }, enumerable: true, configurable: true });
  document.referrer = "";
  document.URL = parts.href;
  document.documentURI = parts.href;
  document.baseURI = parts.href;
  document.domain = parts.hostname;
  document.title = document.title; // leave as-is; real getter/setter already present

  // `document.currentScript`: real browsers return the executing <script> element. We don't
  // track it, so expose a harmless stub element (with a no-op remove()) so inline bootstraps
  // like `document.currentScript.remove()` (TanStack/React hydration) don't throw.
  document.currentScript = {
    remove: fn, setAttribute: fn, getAttribute: function () { return null; },
    removeAttribute: fn, hasAttribute: function () { return false; },
    addEventListener: fn, removeEventListener: fn, appendChild: function (c) { return c; },
    parentNode: null, parentElement: null, nextSibling: null, previousSibling: null,
    src: "", type: "", async: false, defer: false, dataset: {}, style: {},
  };

  function makeEvent(type) {
    return { type: type, target: document, currentTarget: document, bubbles: false, cancelable: false,
             defaultPrevented: false, timeStamp: 0,
             preventDefault: fn, stopPropagation: fn, stopImmediatePropagation: fn };
  }
  function fireOn(target, type) {
    if (target && typeof target.dispatchEvent === "function") {
      try { target.dispatchEvent(makeEvent(type)); } catch (e) { (globalThis.__timerErrors || []).push(String(e)); }
    }
    // Also invoke an `on<type>` handler if one was assigned (e.g. window.onload = ...).
    var on = target ? target["on" + type] : null;
    if (typeof on === "function") {
      try { on.call(target, makeEvent(type)); } catch (e) { (globalThis.__timerErrors || []).push(String(e)); }
    }
  }
  // Called from Rust's drain phase, in order, to advance readyState and fire lifecycle events.
  def(globalThis, "__fireLifecycleEvents", function () {
    readyState = "interactive";
    fireOn(document, "readystatechange");
    fireOn(document, "DOMContentLoaded");
    readyState = "complete";
    fireOn(document, "readystatechange");
    fireOn(window, "load");
    fireOn(document, "load");
    fireOn(window, "pageshow");
  });

  // --- document extras ---------------------------------------------------------------------
  var cookieStore = "";
  Object.defineProperty(document, "cookie", {
    get: function () { return cookieStore; },
    set: function (v) {
      v = String(v);
      var pair = v.split(";")[0];
      if (pair.indexOf("=") >= 0) { cookieStore = cookieStore ? (cookieStore + "; " + pair) : pair; }
    },
    enumerable: true, configurable: true
  });

  // head / documentElement may be missing from the native document; add lazy getters that
  // resolve via querySelector without clobbering existing accessors (e.g. `body`).
  function ensureGetter(name, selector) {
    var d = Object.getOwnPropertyDescriptor(document, name);
    if (d && (d.get || d.value)) { return; }
    Object.defineProperty(document, name, {
      get: function () { try { return document.querySelector(selector); } catch (e) { return null; } },
      enumerable: true, configurable: true
    });
  }
  ensureGetter("head", "head");
  // documentElement / body already exist as native accessors; only add head defensively.

  // getElementsByClassName is now a real native binding on document; nothing to add here.

  // --- write-through style / classList / dataset, backed by the real DOM attrs --------------
  // All three read and write the element's `style` / `class` / `data-*` attributes in the shared
  // document via the native `document.__getAttr/__setAttr/__removeAttr(node, name[, value])`
  // helpers, keyed by the wrapper's hidden `__node` id. This is what makes JS-driven style/class
  // changes survive into the engine's re-cascade and actually re-render.

  // Parse `prop: value; ...` into an ordered list of [prop, value] pairs (lowercased props).
  function parseStyleDecls(text) {
    var out = [];
    text = String(text || "");
    var parts = text.split(";");
    for (var i = 0; i < parts.length; i++) {
      var seg = parts[i];
      var c = seg.indexOf(":");
      if (c < 0) { continue; }
      var name = seg.slice(0, c).trim().toLowerCase();
      var val = seg.slice(c + 1).trim();
      if (name) { out.push([name, val]); }
    }
    return out;
  }
  function serializeStyleDecls(decls) {
    var s = "";
    for (var i = 0; i < decls.length; i++) { s += (s ? " " : "") + decls[i][0] + ": " + decls[i][1] + ";"; }
    return s;
  }
  // camelCase JS property -> kebab-case CSS property (e.g. backgroundColor -> background-color).
  function camelToKebab(p) {
    p = String(p);
    if (p.indexOf("-") >= 0) { return p.toLowerCase(); } // already kebab (e.g. via setProperty)
    // Leading vendor prefix like `webkitTransform` -> `-webkit-transform`.
    var out = p.replace(/([A-Z])/g, function (m) { return "-" + m.toLowerCase(); });
    if (/^(webkit|moz|ms|o)-/.test(out)) { out = "-" + out; }
    return out;
  }
  function kebabToCamel(p) {
    p = String(p);
    return p.replace(/-([a-z])/g, function (_, c) { return c.toUpperCase(); });
  }
  function styleAttr(node) { var v = document.__getAttr(node, "style"); return v == null ? "" : v; }
  function makeStyle(node) {
    function read() { return parseStyleDecls(styleAttr(node)); }
    function find(decls, name) { for (var i = 0; i < decls.length; i++) { if (decls[i][0] === name) { return i; } } return -1; }
    function getVal(name) { var d = read(); var i = find(d, name); return i >= 0 ? d[i][1] : ""; }
    function setVal(name, val) {
      var d = read(); var i = find(d, name);
      if (val == null || val === "") {
        if (i >= 0) { d.splice(i, 1); }
      } else {
        val = String(val);
        if (i >= 0) { d[i][1] = val; } else { d.push([name, val]); }
      }
      document.__setAttr(node, "style", serializeStyleDecls(d));
    }
    var base = {
      getPropertyValue: function (p) { return getVal(String(p).toLowerCase()); },
      getPropertyPriority: function () { return ""; },
      setProperty: function (p, v) { setVal(String(p).toLowerCase(), v); },
      removeProperty: function (p) { p = String(p).toLowerCase(); var old = getVal(p); setVal(p, ""); return old; },
      item: function (i) { var d = read(); return i >= 0 && i < d.length ? d[i][0] : ""; }
    };
    Object.defineProperty(base, "length", { get: function () { return read().length; }, enumerable: false, configurable: true });
    Object.defineProperty(base, "cssText", {
      get: function () { return styleAttr(node); },
      set: function (v) { document.__setAttr(node, "style", serializeStyleDecls(parseStyleDecls(v))); },
      enumerable: true, configurable: true
    });
    try {
      return new Proxy(base, {
        get: function (t, p) {
          if (typeof p !== "string") { return t[p]; }
          if (p in t) { return t[p]; }
          return getVal(camelToKebab(p));
        },
        set: function (t, p, v) {
          if (typeof p !== "string") { t[p] = v; return true; }
          if (p === "cssText") { t.cssText = v; return true; }
          setVal(camelToKebab(p), v); return true;
        }
      });
    } catch (e) { return base; }
  }
  function makeClassList(node) {
    function read() { var c = document.__getAttr(node, "class"); return c ? String(c).split(/\s+/).filter(Boolean) : []; }
    function write(arr) {
      var seen = Object.create(null), out = [];
      for (var i = 0; i < arr.length; i++) { if (!seen[arr[i]]) { seen[arr[i]] = 1; out.push(arr[i]); } }
      document.__setAttr(node, "class", out.join(" "));
    }
    var cl = {
      add: function () { var c = read(); for (var i = 0; i < arguments.length; i++) { var n = String(arguments[i]); if (c.indexOf(n) < 0) c.push(n); } write(c); },
      remove: function () { var c = read(); for (var i = 0; i < arguments.length; i++) { var x = c.indexOf(String(arguments[i])); if (x >= 0) c.splice(x, 1); } write(c); },
      toggle: function (n, force) {
        n = String(n); var c = read(); var x = c.indexOf(n);
        if (force === true) { if (x < 0) { c.push(n); write(c); } return true; }
        if (force === false) { if (x >= 0) { c.splice(x, 1); write(c); } return false; }
        if (x >= 0) { c.splice(x, 1); write(c); return false; } c.push(n); write(c); return true;
      },
      replace: function (oldC, newC) { var c = read(); var x = c.indexOf(String(oldC)); if (x >= 0) { c[x] = String(newC); write(c); return true; } return false; },
      contains: function (n) { return read().indexOf(String(n)) >= 0; },
      item: function (i) { var c = read(); return i >= 0 && i < c.length ? c[i] : null; },
      forEach: function (cb, thisArg) { var c = read(); for (var i = 0; i < c.length; i++) { cb.call(thisArg, c[i], i, this); } },
      toString: function () { return read().join(" "); }
    };
    Object.defineProperty(cl, "length", { get: function () { return read().length; }, enumerable: false, configurable: true });
    Object.defineProperty(cl, "value", {
      get: function () { return read().join(" "); },
      set: function (v) { document.__setAttr(node, "class", String(v)); },
      enumerable: false, configurable: true
    });
    return cl;
  }
  function makeDataset(node) {
    // Live view over data-* attributes. dataset.fooBar <-> data-foo-bar.
    var base = {};
    try {
      return new Proxy(base, {
        get: function (t, p) {
          if (typeof p !== "string") { return t[p]; }
          var v = document.__getAttr(node, "data-" + camelToKebab(p));
          return v == null ? undefined : v;
        },
        set: function (t, p, v) { if (typeof p === "string") { document.__setAttr(node, "data-" + camelToKebab(p), v == null ? "" : String(v)); } return true; },
        deleteProperty: function (t, p) { if (typeof p === "string") { document.__removeAttr(node, "data-" + camelToKebab(p)); } return true; },
        has: function (t, p) { return typeof p === "string" && document.__getAttr(node, "data-" + camelToKebab(p)) != null; }
      });
    } catch (e) { return base; }
  }
  function makeRect() { return { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0, toJSON: function () { return this; } }; }

  // Split CSS source into top-level rules (brace-balanced), returning one normalized cssText per
  // rule. Good enough for feature-detection libraries that read `styleEl.sheet.cssRules[i].cssText`.
  function parseCssRules(css) {
    css = String(css == null ? "" : css);
    var rules = [], depth = 0, start = 0, i = 0, n = css.length;
    for (; i < n; i++) {
      var ch = css[i];
      if (ch === "{") { depth++; }
      else if (ch === "}") {
        depth--;
        if (depth === 0) { var seg = css.slice(start, i + 1).trim(); if (seg) { rules.push(normalizeCssText(seg)); } start = i + 1; }
      }
      else if (ch === ";" && depth === 0) {
        var s2 = css.slice(start, i + 1).trim(); if (s2) { rules.push(normalizeCssText(s2)); } start = i + 1;
      }
    }
    var tail = css.slice(start).trim();
    if (tail) { rules.push(normalizeCssText(tail + (depth > 0 ? "}" : ""))); }
    return rules;
  }
  function normalizeCssText(t) {
    // Collapse internal whitespace and normalize "{ }" spacing so equal rules compare equal.
    t = String(t).replace(/\s+/g, " ").trim();
    t = t.replace(/\s*{\s*/g, " { ").replace(/\s*}\s*/g, " }").replace(/\s*;\s*/g, "; ").trim();
    return t;
  }
  function makeRule(text) {
    return { cssText: text, type: 1, selectorText: (String(text).split("{")[0] || "").trim(),
             cssRules: [], parentRule: null, parentStyleSheet: null };
  }
  function makeStyleSheet(styleEl) {
    var ss = {
      type: "text/css", disabled: false, href: null, title: null, media: { length: 0 },
      ownerNode: styleEl, parentStyleSheet: null,
      get cssRules() { var rs = parseCssRules(styleEl.textContent).map(makeRule); rs.item = function (i) { return this[i] || null; }; return rs; },
      insertRule: function (rule, index) {
        var t = styleEl.textContent || ""; styleEl.textContent = (index ? t : "") + String(rule) + (index ? "" : t); return index || 0;
      },
      deleteRule: function () {},
      replaceSync: function (text) { styleEl.textContent = String(text); }
    };
    Object.defineProperty(ss, "rules", { get: function () { return this.cssRules; }, enumerable: false, configurable: true });
    return ss;
  }

  // --- per-node wrapper cache (stable identity + expando persistence) ----------------------
  // Native DOM methods/accessors return a FRESH wrapper object on every call (each carrying the
  // hidden `__node` id). Frameworks like Vue stash internal state directly on DOM nodes
  // (`el.__vnode`, `el._vei`, `el.$once`, ...) and rely on `getElementById(x) === getElementById(x)`
  // and on those expandos surviving across lookups. To honor that we keep a JS-side map from node
  // id -> the one canonical enriched wrapper, and route every element the native layer hands back
  // through `canon()`, which returns the cached wrapper (copying over the fresh wrapper's own
  // function bindings on first sight). The cache lives entirely on the JS side, so Boa's GC roots
  // the wrappers for us — no Boa values are held in Rust (same discipline as elsewhere).
  var __nodeCache = Object.create(null);
  function canon(el) {
    if (!el || typeof el !== "object") { return el; }
    var node = el.__node;
    if (typeof node !== "number") { return enrichElement(el); }
    var cached = __nodeCache[node];
    if (cached) { return cached; }
    __nodeCache[node] = el;       // record BEFORE enriching so re-entrant lookups dedupe
    enrichElement(el);
    return el;
  }
  def(globalThis, "__canonNode", canon);

  // Map a tag name to the most specific DOM interface prototype we have, so element wrappers
  // satisfy `el instanceof HTMLElement/Element/Node` (and SVG/MathML where appropriate). The
  // wrapper keeps all its own (native) accessors/methods; we only graft the interface prototype
  // onto its chain via Object.setPrototypeOf, then re-install its own data/accessor props (they
  // are own properties on the wrapper, so the chain swap doesn't lose them).
  var svgTags = { svg: 1, path: 1, g: 1, rect: 1, circle: 1, ellipse: 1, line: 1, polyline: 1,
    polygon: 1, text: 1, tspan: 1, defs: 1, use: 1, symbol: 1, marker: 1, "clippath": 1,
    mask: 1, pattern: 1, image: 1, "lineargradient": 1, "radialgradient": 1, stop: 1, filter: 1,
    foreignobject: 1 };
  var tagIface = {
    div: "HTMLDivElement", span: "HTMLSpanElement", p: "HTMLParagraphElement", a: "HTMLAnchorElement",
    img: "HTMLImageElement", input: "HTMLInputElement", button: "HTMLButtonElement",
    select: "HTMLSelectElement", option: "HTMLOptionElement", textarea: "HTMLTextAreaElement",
    form: "HTMLFormElement", label: "HTMLLabelElement", ul: "HTMLUListElement", ol: "HTMLOListElement",
    li: "HTMLLIElement", table: "HTMLTableElement", tr: "HTMLTableRowElement", td: "HTMLTableCellElement",
    th: "HTMLTableCellElement", canvas: "HTMLCanvasElement", video: "HTMLVideoElement",
    audio: "HTMLAudioElement", iframe: "HTMLIFrameElement", template: "HTMLTemplateElement",
    h1: "HTMLHeadingElement", h2: "HTMLHeadingElement", h3: "HTMLHeadingElement",
    h4: "HTMLHeadingElement", h5: "HTMLHeadingElement", h6: "HTMLHeadingElement",
    body: "HTMLBodyElement", html: "HTMLHtmlElement", head: "HTMLHeadElement",
    script: "HTMLScriptElement", style: "HTMLStyleElement", link: "HTMLLinkElement",
    meta: "HTMLMetaElement", title: "HTMLTitleElement"
  };
  function ifaceProtoForTag(tag) {
    tag = String(tag || "").toLowerCase();
    if (svgTags[tag]) { return (globalThis.SVGElement && globalThis.SVGElement.prototype) || null; }
    var name = tagIface[tag];
    var ctor = name && globalThis[name];
    if (typeof ctor === "function" && ctor.prototype) { return ctor.prototype; }
    return (globalThis.HTMLElement && globalThis.HTMLElement.prototype) || null;
  }

  function enrichElement(el) {
    if (!el || typeof el !== "object") { return el; }
    if (el.__enriched) { return el; }
    var node = el.__node;
    def(el, "__enriched", true);
    // Graft the matching DOM interface prototype onto the wrapper's chain (own props survive).
    if (typeof node === "number") {
      try {
        var tag = el.tagName;
        var proto = ifaceProtoForTag(tag);
        if (proto && Object.getPrototypeOf(el) !== proto) { Object.setPrototypeOf(el, proto); }
      } catch (e) {}
    }
    if (typeof node === "number") {
      def(el, "style", makeStyle(node));
      def(el, "classList", makeClassList(node));
      def(el, "dataset", makeDataset(node));
      // Form-control `value` / `checked` reflection: back them by element ATTRIBUTES so that
      // reading/writing `el.value` (and `el.checked`) is visible to layout, which renders the
      // input's text from the `value` attribute. Only for <input>/<textarea>/<select>; guard so
      // page-defined accessors aren't clobbered.
      try {
        var __formTag = typeof el.tagName === "string" ? el.tagName.toLowerCase() : "";
        if (__formTag === "input" || __formTag === "textarea" || __formTag === "select") {
          var __hasValue = false;
          try { var __vd = Object.getOwnPropertyDescriptor(el, "value"); __hasValue = !!(__vd && (__vd.get || __vd.set)); } catch (e8) {}
          if (!__hasValue) {
            Object.defineProperty(el, "value", {
              get: function () { var v = __getAttr(node, "value"); return v == null ? "" : String(v); },
              set: function (v) { __setAttr(node, "value", String(v == null ? "" : v)); },
              configurable: true, enumerable: true
            });
          }
          // `checked` for checkbox/radio inputs, backed by presence of the `checked` attribute.
          if (__formTag === "input") {
            var __ty = String(__getAttr(node, "type") || "").toLowerCase();
            if (__ty === "checkbox" || __ty === "radio") {
              var __hasChecked = false;
              try { var __cd = Object.getOwnPropertyDescriptor(el, "checked"); __hasChecked = !!(__cd && (__cd.get || __cd.set)); } catch (e9) {}
              if (!__hasChecked) {
                Object.defineProperty(el, "checked", {
                  get: function () { return __getAttr(node, "checked") != null; },
                  set: function (v) { if (v) { __setAttr(node, "checked", ""); } else { __removeAttr(node, "checked"); } },
                  configurable: true, enumerable: true
                });
              }
            }
          }
        }
      } catch (e10) {}
    } else {
      // Detached/foreign object: fall back to inert stubs so access doesn't throw.
      if (!("style" in el) || el.style == null) { def(el, "style", { getPropertyValue: function () { return ""; }, setProperty: fn, removeProperty: function () { return ""; }, cssText: "" }); }
      if (!("classList" in el) || el.classList == null) { def(el, "classList", { add: fn, remove: fn, toggle: function () { return false; }, contains: function () { return false; }, item: function () { return null; } }); }
      if (!("dataset" in el) || el.dataset == null) { def(el, "dataset", {}); }
    }
    // Element-returning native methods hand back un-enriched wrappers; wrap them so the result
    // is enriched (gets style/classList/dataset) before page code touches it.
    var elemMethods = ["querySelector", "closest"];
    for (var mi = 0; mi < elemMethods.length; mi++) {
      (function (mn) {
        var orig = el[mn];
        if (typeof orig === "function") { def(el, mn, function () { return canon(orig.apply(this, arguments)); }); }
      })(elemMethods[mi]);
    }
    var listMethods = ["querySelectorAll", "getElementsByTagName", "getElementsByClassName"];
    for (var li = 0; li < listMethods.length; li++) {
      (function (mn) {
        var orig = el[mn];
        if (typeof orig === "function") { def(el, mn, function () { var r = orig.apply(this, arguments); if (r && typeof r.length === "number") { for (var i = 0; i < r.length; i++) { r[i] = canon(r[i]); } } return r; }); }
      })(listMethods[mi]);
    }
    // Navigation accessors return fresh wrappers each time; re-wrap to canonicalize on read.
    var navAccessors = ["parentNode", "parentElement", "firstChild", "lastChild", "firstElementChild",
                        "nextSibling", "previousSibling", "nextElementSibling", "previousElementSibling"];
    for (var ni = 0; ni < navAccessors.length; ni++) {
      (function (an) {
        var d = Object.getOwnPropertyDescriptor(el, an);
        if (d && d.get) { var og = d.get; Object.defineProperty(el, an, { get: function () { return canon(og.call(this)); }, configurable: true, enumerable: d.enumerable }); }
      })(navAccessors[ni]);
    }
    var listAccessors = ["children", "childNodes"];
    for (var ci = 0; ci < listAccessors.length; ci++) {
      (function (an) {
        var d = Object.getOwnPropertyDescriptor(el, an);
        if (d && d.get) { var og = d.get; Object.defineProperty(el, an, { get: function () { var r = og.call(this); if (r && typeof r.length === "number") { for (var i = 0; i < r.length; i++) { r[i] = canon(r[i]); } } return r; }, configurable: true, enumerable: d.enumerable }); }
      })(listAccessors[ci]);
    }

    // <style> (and stylesheet <link>) expose a live CSSStyleSheet via `.sheet`.
    if (typeof el.tagName === "string" && (el.tagName.toLowerCase() === "style" || el.tagName.toLowerCase() === "link") && !("sheet" in el)) {
      var __sheet = null;
      Object.defineProperty(el, "sheet", { get: function () { if (!__sheet) { __sheet = makeStyleSheet(this); } return __sheet; }, configurable: true, enumerable: false });
    }

    if (typeof el.getBoundingClientRect !== "function") { def(el, "getBoundingClientRect", makeRect); }
    if (typeof el.getClientRects !== "function") { def(el, "getClientRects", function () { return []; }); }
    if (typeof el.scrollIntoView !== "function") { def(el, "scrollIntoView", fn); }
    if (typeof el.focus !== "function") { def(el, "focus", fn); }
    if (typeof el.blur !== "function") { def(el, "blur", fn); }
    if (typeof el.click !== "function") { def(el, "click", fn); }
    if (typeof el.cloneNode !== "function") { def(el, "cloneNode", function () { return this; }); }
    if (typeof el.hasChildNodes !== "function") { def(el, "hasChildNodes", function () { try { return (this.childNodes || []).length > 0; } catch (e) { return false; } }); }
    if (!("nodeType" in el)) { def(el, "nodeType", 1); }
    if (!("ownerDocument" in el)) { def(el, "ownerDocument", document); }
    if (!("scrollTop" in el)) { el.scrollTop = 0; }
    if (!("scrollLeft" in el)) { el.scrollLeft = 0; }
    if (!("offsetWidth" in el)) { el.offsetWidth = 0; }
    if (!("offsetHeight" in el)) { el.offsetHeight = 0; }
    if (!("clientWidth" in el)) { el.clientWidth = 0; }
    if (!("clientHeight" in el)) { el.clientHeight = 0; }
    // SVG geometry properties expose SVGAnimatedLength / SVGAnimatedRect objects whose `.baseVal`
    // pages read (e.g. favicon generators do `svg.width.baseVal.value`). Provide zeroed stubs so
    // those reads don't throw. Gated on SVG tags so HTML elements keep their own width/height attrs.
    try {
      var __svgTag = typeof el.tagName === "string" ? el.tagName.toLowerCase() : "";
      if (svgTags[__svgTag]) {
        var __len = ["width", "height", "x", "y"];
        for (var __si = 0; __si < __len.length; __si++) {
          (function (p) {
            if (!(p in el)) { def(el, p, { baseVal: { value: 0, valueAsString: "0", valueInSpecifiedUnits: 0 }, animVal: { value: 0 } }); }
          })(__len[__si]);
        }
        if (!("viewBox" in el)) { def(el, "viewBox", { baseVal: { x: 0, y: 0, width: 0, height: 0 }, animVal: { x: 0, y: 0, width: 0, height: 0 } }); }
        if (!("preserveAspectRatio" in el)) { def(el, "preserveAspectRatio", { baseVal: { align: 0, meetOrSlice: 0 }, animVal: { align: 0, meetOrSlice: 0 } }); }
      }
    } catch (e) {}
    // <canvas>: provide a non-throwing 2D context stub (we don't rasterize canvas, but pages
    // feature-detect and measure text through it). '2d' returns a stub whose methods are no-ops
    // and whose measureText returns an approximate width; 'webgl'/'webgl2' return null so callers
    // fall back gracefully.
    try {
      var __cvTag = typeof el.tagName === "string" ? el.tagName.toLowerCase() : "";
      if (__cvTag === "canvas" && typeof el.getContext !== "function") {
        if (!("width" in el)) { def(el, "width", 300); }
        if (!("height" in el)) { def(el, "height", 150); }
        var noop = function () {};
        def(el, "getContext", function (type) {
          if (type !== "2d") { return null; }
          if (el.__ctx2d) { return el.__ctx2d; }
          var ctx = {
            canvas: el, fillStyle: '#000', strokeStyle: '#000', lineWidth: 1, lineCap: "butt",
            lineJoin: "miter", miterLimit: 10, font: "10px sans-serif", textAlign: "start",
            textBaseline: "alphabetic", direction: "ltr", globalAlpha: 1,
            globalCompositeOperation: "source-over", imageSmoothingEnabled: true,
            shadowBlur: 0, shadowColor: "rgba(0,0,0,0)", shadowOffsetX: 0, shadowOffsetY: 0,
            save: noop, restore: noop, scale: noop, rotate: noop, translate: noop, transform: noop,
            setTransform: noop, resetTransform: noop, getTransform: function () { return {}; },
            beginPath: noop, closePath: noop, moveTo: noop, lineTo: noop, bezierCurveTo: noop,
            quadraticCurveTo: noop, arc: noop, arcTo: noop, ellipse: noop, rect: noop, roundRect: noop,
            fill: noop, stroke: noop, clip: noop, isPointInPath: function () { return false; },
            isPointInStroke: function () { return false; }, fillRect: noop, strokeRect: noop,
            clearRect: noop, fillText: noop, strokeText: noop,
            measureText: function (s) { var w = (s ? String(s).length : 0) * 6; return { width: w, actualBoundingBoxLeft: 0, actualBoundingBoxRight: w, actualBoundingBoxAscent: 8, actualBoundingBoxDescent: 2, fontBoundingBoxAscent: 8, fontBoundingBoxDescent: 2 }; },
            setLineDash: noop, getLineDash: function () { return []; }, drawImage: noop, drawFocusIfNeeded: noop,
            createImageData: function (w, h) { var ww = w | 0, hh = h | 0; return { width: ww, height: hh, data: new Uint8ClampedArray(ww * hh * 4) }; },
            getImageData: function (x, y, w, h) { var ww = w | 0, hh = h | 0; return { width: ww, height: hh, data: new Uint8ClampedArray(ww * hh * 4) }; },
            putImageData: noop,
            createLinearGradient: function () { return { addColorStop: noop }; },
            createRadialGradient: function () { return { addColorStop: noop }; },
            createConicGradient: function () { return { addColorStop: noop }; },
            createPattern: function () { return null; },
            getContextAttributes: function () { return { alpha: true, desynchronized: false, colorSpace: "srgb", willReadFrequently: false }; },
          };
          def(el, "__ctx2d", ctx);
          return ctx;
        });
        if (typeof el.toDataURL !== "function") { def(el, "toDataURL", function () { return "data:,"; }); }
        if (typeof el.toBlob !== "function") { def(el, "toBlob", function (cb) { if (typeof cb === "function") { cb(null); } }); }
      }
    } catch (e) {}
    installEvents(el);
    return el;
  }
  // Expose so element-returning native accessors (parentNode, etc.) can be enriched lazily by
  // anything that needs it. (Kept non-enumerable.)
  def(globalThis, "__enrichElement", enrichElement);

  function wrapReturningElement(obj, name) {
    var orig = obj[name];
    if (typeof orig !== "function") { return; }
    def(obj, name, function () {
      var r = orig.apply(this, arguments);
      if (r && typeof r === "object") {
        if (typeof r.length === "number" && typeof r.splice === "function") {
          for (var i = 0; i < r.length; i++) { r[i] = canon(r[i]); }
        } else {
          return canon(r);
        }
      }
      return r;
    });
  }
  wrapReturningElement(document, "createElement");
  wrapReturningElement(document, "getElementById");
  wrapReturningElement(document, "getElementsByTagName");
  wrapReturningElement(document, "getElementsByClassName");
  wrapReturningElement(document, "querySelector");
  wrapReturningElement(document, "querySelectorAll");

  // createElementNS(ns, qualifiedName) — used by Vue/SVG. There is no namespaced node in the
  // DOM arena, so create a normal element from the local name (dropping any prefix) and record
  // the namespace so namespace-aware code can read it back. The element is fully enriched via
  // document.createElement above (appendChild/setAttribute/etc. all present).
  if (typeof document.createElementNS !== "function") {
    def(document, "createElementNS", function (ns, qualifiedName) {
      var name = String(qualifiedName == null ? "" : qualifiedName);
      var local = name.indexOf(":") >= 0 ? name.slice(name.indexOf(":") + 1) : name;
      var el = document.createElement(local);
      try { def(el, "namespaceURI", ns == null ? null : String(ns)); } catch (e) {}
      return el;
    });
  }

  // Enrich element wrappers returned by the native element-navigation accessors and methods.
  // These return fresh wrapper objects each time, so wrap the prototype-less accessors by
  // intercepting via getter wrappers is impractical; instead wrap the element-returning methods
  // on a per-element basis when an element is first enriched. We patch the document-level
  // accessors (body/documentElement/head) below.
  function enrichDocAccessor(name) {
    var d = Object.getOwnPropertyDescriptor(document, name);
    if (!d || !d.get) { return; }
    var origGet = d.get;
    Object.defineProperty(document, name, {
      get: function () { return canon(origGet.call(this)); },
      enumerable: d.enumerable, configurable: true
    });
  }
  enrichDocAccessor("body");
  enrichDocAccessor("documentElement");
  enrichDocAccessor("head");

  // --- document node-creation helpers ------------------------------------------------------
  // createTextNode / createComment / createDocumentFragment return lightweight node-ish objects.
  // They aren't backed by the real DOM arena (only createElement is), but they are appendable to
  // real elements as no-ops and carry the properties scripts read, so init code doesn't throw.
  // Back text + comment nodes with REAL arena nodes (via the native primitives + __wrapNode) so
  // they have a working parentNode / insertBefore / sibling chain. Vue uses comment + text nodes as
  // fragment anchors and re-reads their parent on every re-render; a detached stub would make
  // `parent.insertBefore(...)` throw (`parent` === null) during a component update.
  if (typeof document.createTextNode !== "function") {
    def(document, "createTextNode", function (data) {
      return __wrapNode(__createText(String(data == null ? "" : data)));
    });
  }
  if (typeof document.createComment !== "function") {
    def(document, "createComment", function (data) {
      return __wrapNode(__createComment(String(data == null ? "" : data)));
    });
  }
  if (typeof document.createDocumentFragment !== "function") {
    def(document, "createDocumentFragment", function () {
      var kids = [];
      return { nodeType: 11, nodeName: "" + String.fromCharCode(35) + "document-fragment", childNodes: kids,
               appendChild: function (c) { kids.push(c); return c; },
               querySelector: function () { return null; }, querySelectorAll: function () { return []; },
               cloneNode: function () { return this; }, get firstChild() { return kids[0] || null; },
               get lastChild() { return kids[kids.length - 1] || null; }, get children() { return kids; } };
    });
  }
  // document.implementation.createHTMLDocument — used to build/parse HTML off to the side (e.g.
  // sanitizers, template parsing). We back it with real (detached) arena nodes so innerHTML /
  // appendChild / querySelector work on the returned document's tree.
  if (typeof document.implementation === "undefined" || !document.implementation) {
    def(document, "implementation", {
      hasFeature: function () { return true; },
      createDocumentType: function (name, pub, sys) { return { nodeType: 10, name: String(name), publicId: pub || "", systemId: sys || "" }; },
      createHTMLDocument: function (title) {
        var htmlEl = document.createElement("html");
        var headEl = document.createElement("head");
        var bodyEl = document.createElement("body");
        htmlEl.appendChild(headEl); htmlEl.appendChild(bodyEl);
        if (title !== undefined && title !== null) {
          var t = document.createElement("title"); t.textContent = String(title); headEl.appendChild(t);
        }
        return {
          nodeType: 9, documentElement: htmlEl, head: headEl, body: bodyEl, title: title ? String(title) : "",
          createElement: function (tag) { return document.createElement(tag); },
          createElementNS: function (ns, tag) { return document.createElementNS ? document.createElementNS(ns, tag) : document.createElement(tag); },
          createTextNode: function (s) { return document.createTextNode(s); },
          createComment: function (s) { return document.createComment(s); },
          createDocumentFragment: function () { return document.createDocumentFragment(); },
          importNode: function (n) { return n; }, adoptNode: function (n) { return n; },
          getElementById: function (id) { return htmlEl.querySelector ? htmlEl.querySelector('#' + id) : null; },
          querySelector: function (s) { return htmlEl.querySelector ? htmlEl.querySelector(s) : null; },
          querySelectorAll: function (s) { return htmlEl.querySelectorAll ? htmlEl.querySelectorAll(s) : []; },
          getElementsByTagName: function (t) { return htmlEl.getElementsByTagName ? htmlEl.getElementsByTagName(t) : []; },
        };
      },
      createDocument: function () { return this.createHTMLDocument(""); },
    });
  }
  if (typeof document.getElementsByName !== "function") {
    def(document, "getElementsByName", function (n) { try { return document.querySelectorAll('[name="' + String(n) + '"]'); } catch (e) { return []; } });
  }
  if (typeof document.contains !== "function") {
    def(document, "contains", function (node) { try { return document.documentElement ? (document.documentElement === node || document.documentElement.contains(node)) : false; } catch (e) { return false; } });
  }
  if (typeof document.createEvent !== "function") {
    def(document, "createEvent", function () { var e = { type: "", bubbles: false, cancelable: false, defaultPrevented: false, preventDefault: fn, stopPropagation: fn, initEvent: function (t, b, c) { this.type = String(t); this.bubbles = !!b; this.cancelable = !!c; }, initCustomEvent: function (t, b, c, d) { this.type = String(t); this.bubbles = !!b; this.cancelable = !!c; this.detail = d; } }; return e; });
  }
  if (typeof document.elementFromPoint !== "function") { def(document, "elementFromPoint", function () { return null; }); }
  if (typeof document.hasFocus !== "function") { def(document, "hasFocus", function () { return true; }); }
  if (!("activeElement" in document)) { Object.defineProperty(document, "activeElement", { get: function () { try { return document.body; } catch (e) { return null; } }, enumerable: true, configurable: true }); }
  if (!("visibilityState" in document)) { document.visibilityState = "visible"; }
  if (!("hidden" in document)) { document.hidden = false; }
  if (!("characterSet" in document)) { document.characterSet = "UTF-8"; }
  if (!("compatMode" in document)) { document.compatMode = "CSS1Compat"; }
  if (!("scrollingElement" in document)) { Object.defineProperty(document, "scrollingElement", { get: function () { try { return document.documentElement; } catch (e) { return null; } }, enumerable: true, configurable: true }); }
  if (typeof document.querySelectorAll === "function" && typeof document.querySelectorAll.call === "function") { /* present */ }

  // --- document.fonts (FontFaceSet) --------------------------------------------------------
  if (!("fonts" in document) || document.fonts == null) {
    var fontFaces = {
      status: "loaded", size: 0,
      ready: Promise.resolve(),
      load: function () { return Promise.resolve([]); },
      check: function () { return true; },
      add: fn, delete: function () { return false; }, has: function () { return false; },
      clear: fn, forEach: fn,
      addEventListener: fn, removeEventListener: fn, dispatchEvent: function () { return false; },
      onloading: null, onloadingdone: null, onloadingerror: null
    };
    // `ready` should be a thenable that resolves to the set itself (per spec resolves to the
    // FontFaceSet). Make it resolve to the set without creating a cycle in JSON paths.
    fontFaces.ready = Promise.resolve(fontFaces);
    Object.defineProperty(document, "fonts", { value: fontFaces, enumerable: false, configurable: true, writable: true });
  }

  // --- Observer constructors (presence + no-op observe/disconnect/takeRecords) --------------
  function makeObserver(name) {
    def(globalThis, name, function (cb) {
      this.callback = typeof cb === "function" ? cb : fn;
      this.observe = fn; this.unobserve = fn; this.disconnect = fn;
      this.takeRecords = function () { return []; };
    });
  }
  if (typeof globalThis.MutationObserver !== "function") { makeObserver("MutationObserver"); }
  if (typeof globalThis.IntersectionObserver !== "function") {
    def(globalThis, "IntersectionObserver", function (cb, opts) {
      this.callback = typeof cb === "function" ? cb : fn;
      this.root = (opts && opts.root) || null; this.rootMargin = (opts && opts.rootMargin) || "0px";
      this.thresholds = (opts && [].concat(opts.threshold || 0)) || [0];
      this.observe = fn; this.unobserve = fn; this.disconnect = fn; this.takeRecords = function () { return []; };
    });
  }
  if (typeof globalThis.ResizeObserver !== "function") { makeObserver("ResizeObserver"); }
  if (typeof globalThis.PerformanceObserver !== "function") {
    def(globalThis, "PerformanceObserver", function (cb) {
      this.callback = typeof cb === "function" ? cb : fn;
      this.observe = fn; this.disconnect = fn; this.takeRecords = function () { return []; };
    });
    globalThis.PerformanceObserver.supportedEntryTypes = [];
  }

  // --- performance -------------------------------------------------------------------------
  if (!globalThis.performance || typeof globalThis.performance.now !== "function") {
    var perfStart = 0;
    globalThis.performance = {
      now: function () { try { return (globalThis.__eventLoop ? globalThis.__eventLoop.now : 0); } catch (e) { return 0; } },
      timeOrigin: 0,
      timing: { navigationStart: 0, fetchStart: 0, domLoading: 0, domInteractive: 0, domContentLoadedEventStart: 0, domContentLoadedEventEnd: 0, domComplete: 0, loadEventStart: 0, loadEventEnd: 0, responseStart: 0, responseEnd: 0, requestStart: 0, connectStart: 0, connectEnd: 0, secureConnectionStart: 0, domainLookupStart: 0, domainLookupEnd: 0, unloadEventStart: 0, unloadEventEnd: 0, redirectStart: 0, redirectEnd: 0 },
      navigation: { type: 0, redirectCount: 0 },
      memory: { usedJSHeapSize: 0, totalJSHeapSize: 0, jsHeapSizeLimit: 0 },
      getEntries: function () { return []; },
      getEntriesByType: function () { return []; },
      getEntriesByName: function () { return []; },
      mark: fn, measure: fn, clearMarks: fn, clearMeasures: fn, clearResourceTimings: fn,
      setResourceTimingBufferSize: fn,
      toJSON: function () { return {}; }
    };
  }

  // --- IdleDeadline-style object is already provided via requestIdleCallback above. ---------

  // --- a few more constructors pages feature-detect ----------------------------------------
  if (typeof globalThis.DOMParser !== "function") {
    def(globalThis, "DOMParser", function () { this.parseFromString = function () { return document; }; });
  }
  if (typeof globalThis.IntersectionObserverEntry !== "function") { def(globalThis, "IntersectionObserverEntry", function () {}); }
  if (typeof globalThis.MutationRecord !== "function") { def(globalThis, "MutationRecord", function () {}); }
  // --- DOM interface constructors / class hierarchy ----------------------------------------
  // Vue (and most frameworks) do `el instanceof SVGElement`, read `Node.prototype`, check
  // `typeof HTMLElement === "function"`, and reference HTMLUnknownElement/Text/Comment/etc.
  // We define each as a real constructor function carrying a `.prototype` and wire up a
  // prototype chain (HTMLDivElement -> HTMLElement -> Element -> Node) so prototype walks and
  // `instanceof` checks behave. The element wrappers' prototype is set to HTMLElement.prototype
  // (see __wrapNode below) so `el instanceof HTMLElement/Element/Node` returns true.
  function defClass(name, parentCtor) {
    if (typeof globalThis[name] === "function") { return globalThis[name]; }
    var ctor = function () {};
    if (parentCtor && parentCtor.prototype) {
      try { Object.setPrototypeOf(ctor.prototype, parentCtor.prototype); } catch (e) {}
    }
    def(globalThis, name, ctor);
    return ctor;
  }
  var NodeCtor = defClass("Node");
  NodeCtor.ELEMENT_NODE = 1; NodeCtor.ATTRIBUTE_NODE = 2; NodeCtor.TEXT_NODE = 3;
  NodeCtor.CDATA_SECTION_NODE = 4; NodeCtor.PROCESSING_INSTRUCTION_NODE = 7; NodeCtor.COMMENT_NODE = 8;
  NodeCtor.DOCUMENT_NODE = 9; NodeCtor.DOCUMENT_TYPE_NODE = 10; NodeCtor.DOCUMENT_FRAGMENT_NODE = 11;
  defClass("EventTarget");
  defClass("CharacterData", NodeCtor);
  defClass("Text", globalThis.CharacterData);
  defClass("Comment", globalThis.CharacterData);
  defClass("CDATASection", globalThis.Text);
  defClass("ProcessingInstruction", globalThis.CharacterData);
  defClass("DocumentFragment", NodeCtor);
  defClass("ShadowRoot", globalThis.DocumentFragment);
  defClass("DocumentType", NodeCtor);
  defClass("Attr", NodeCtor);
  var ElementCtor = defClass("Element", NodeCtor);
  var HTMLElementCtor = defClass("HTMLElement", ElementCtor);
  defClass("SVGElement", ElementCtor);
  defClass("SVGSVGElement", globalThis.SVGElement);
  defClass("SVGGraphicsElement", globalThis.SVGElement);
  defClass("MathMLElement", ElementCtor);
  defClass("HTMLUnknownElement", HTMLElementCtor);
  // A broad set of concrete HTMLElement subclasses pages feature-detect / reference.
  var htmlSubclasses = [
    "HTMLDivElement", "HTMLSpanElement", "HTMLParagraphElement", "HTMLAnchorElement",
    "HTMLImageElement", "HTMLInputElement", "HTMLButtonElement", "HTMLSelectElement",
    "HTMLOptionElement", "HTMLOptGroupElement", "HTMLTextAreaElement", "HTMLFormElement",
    "HTMLLabelElement", "HTMLUListElement", "HTMLOListElement", "HTMLLIElement",
    "HTMLTableElement", "HTMLTableRowElement", "HTMLTableCellElement", "HTMLTableSectionElement",
    "HTMLTableColElement", "HTMLTableCaptionElement", "HTMLHeadingElement", "HTMLPreElement",
    "HTMLQuoteElement", "HTMLHRElement", "HTMLBRElement", "HTMLScriptElement",
    "HTMLStyleElement", "HTMLLinkElement", "HTMLMetaElement", "HTMLTitleElement",
    "HTMLHeadElement", "HTMLBodyElement", "HTMLHtmlElement", "HTMLCanvasElement",
    "HTMLVideoElement", "HTMLAudioElement", "HTMLMediaElement", "HTMLSourceElement",
    "HTMLTrackElement", "HTMLIFrameElement", "HTMLEmbedElement", "HTMLObjectElement",
    "HTMLPictureElement", "HTMLTemplateElement", "HTMLSlotElement", "HTMLDataListElement",
    "HTMLFieldSetElement", "HTMLLegendElement", "HTMLDetailsElement", "HTMLDialogElement",
    "HTMLMenuElement", "HTMLMapElement", "HTMLAreaElement", "HTMLDListElement",
    "HTMLDataElement", "HTMLTimeElement", "HTMLOutputElement", "HTMLProgressElement",
    "HTMLMeterElement", "HTMLModElement", "HTMLFontElement", "HTMLDirectoryElement",
    "HTMLMarqueeElement"
  ];
  // HTMLMediaElement should sit under HTMLElement; audio/video under it. Keep flat-under-HTMLElement
  // for simplicity except a couple that pages explicitly chain.
  for (var hi = 0; hi < htmlSubclasses.length; hi++) { defClass(htmlSubclasses[hi], HTMLElementCtor); }

  // Document / Window and the other DOM interface constructors pages reference as globals
  // (e.g. `x instanceof Document`, `Node.prototype`, `HTMLCollection`). Defined so references and
  // instanceof checks don't throw ReferenceError.
  var DocumentCtor = defClass("Document", NodeCtor);
  defClass("HTMLDocument", DocumentCtor);
  defClass("XMLDocument", DocumentCtor);
  defClass("Window", globalThis.EventTarget);
  defClass("AbstractRange"); defClass("Range", globalThis.AbstractRange); defClass("StaticRange", globalThis.AbstractRange);
  var domIfaces = [
    "HTMLCollection", "NodeList", "DOMTokenList", "NamedNodeMap", "DOMStringMap", "DOMRectList",
    "CSSStyleDeclaration", "StyleSheet", "CSSStyleSheet", "StyleSheetList", "MediaList",
    "CSSRule", "CSSStyleRule", "CSSMediaRule", "CSSKeyframesRule", "CSSKeyframeRule",
    "CSSImportRule", "CSSFontFaceRule", "CSSSupportsRule", "CSSGroupingRule",
    "DOMRect", "DOMRectReadOnly", "DOMPoint", "DOMPointReadOnly", "DOMMatrix", "DOMMatrixReadOnly",
    "DOMQuad", "DOMException", "DOMParser", "XMLSerializer", "XPathResult", "XPathEvaluator",
    "MutationRecord", "AnimationEffect", "KeyframeEffect", "Animation", "AnimationTimeline",
    "CSSStyleValue", "StylePropertyMap", "VisualViewport", "Selection", "TextMetrics",
    "TimeRanges", "ValidityState", "HTMLFormControlsCollection", "RadioNodeList",
  ];
  for (var di = 0; di < domIfaces.length; di++) { defClass(domIfaces[di]); }

  // --- Image / Audio / media element constructors ------------------------------------------
  if (typeof globalThis.Image !== "function") {
    def(globalThis, "Image", function (w, h) {
      this.width = w || 0; this.height = h || 0; this.naturalWidth = 0; this.naturalHeight = 0;
      this.complete = false; this.src = ""; this.alt = ""; this.crossOrigin = null; this.decoding = "auto";
      this.onload = null; this.onerror = null;
      this.setAttribute = fn; this.getAttribute = function () { return null; };
      this.addEventListener = fn; this.removeEventListener = fn; this.dispatchEvent = function () { return false; };
      this.decode = function () { return Promise.resolve(); };
      try { def(this, "style", { setProperty: fn, getPropertyValue: function () { return ""; }, removeProperty: function () { return ""; }, cssText: "" }); } catch (e) {}
    });
    def(globalThis, "HTMLImageElement", globalThis.Image);
  }
  if (typeof globalThis.Audio !== "function") {
    def(globalThis, "Audio", function (src) {
      this.src = src || ""; this.currentTime = 0; this.paused = true; this.volume = 1;
      this.play = function () { return Promise.resolve(); }; this.pause = fn; this.load = fn;
      this.canPlayType = function () { return ""; };
      this.addEventListener = fn; this.removeEventListener = fn;
    });
  }
  // --- Blob / File / FileReader / Worker presence stubs ------------------------------------
  if (typeof globalThis.Blob !== "function") {
    def(globalThis, "Blob", function (parts, opts) {
      this.size = 0; this.type = (opts && opts.type) || "";
      this.slice = function () { return new globalThis.Blob([], { type: this.type }); };
      this.text = function () { return Promise.resolve(""); };
      this.arrayBuffer = function () { return Promise.resolve(new ArrayBuffer(0)); };
    });
  }
  if (typeof globalThis.File !== "function") {
    def(globalThis, "File", function (parts, name, opts) { globalThis.Blob.call(this, parts, opts); this.name = String(name || ""); this.lastModified = 0; });
  }
  if (typeof globalThis.FileReader !== "function") {
    def(globalThis, "FileReader", function () {
      this.readyState = 0; this.result = null; this.error = null;
      this.onload = null; this.onloadend = null; this.onerror = null;
      this.readAsText = fn; this.readAsDataURL = fn; this.readAsArrayBuffer = fn; this.abort = fn;
      this.addEventListener = fn; this.removeEventListener = fn;
    });
  }
  if (typeof globalThis.Worker !== "function") {
    def(globalThis, "Worker", function () { this.postMessage = fn; this.terminate = fn; this.onmessage = null; this.onerror = null; this.addEventListener = fn; this.removeEventListener = fn; });
  }
  if (typeof globalThis.WebSocket !== "function") {
    def(globalThis, "WebSocket", function () { this.readyState = 3; this.send = fn; this.close = fn; this.onopen = null; this.onmessage = null; this.onerror = null; this.onclose = null; this.addEventListener = fn; this.removeEventListener = fn; });
    globalThis.WebSocket.CONNECTING = 0; globalThis.WebSocket.OPEN = 1; globalThis.WebSocket.CLOSING = 2; globalThis.WebSocket.CLOSED = 3;
  }
  if (typeof globalThis.Headers !== "function") {
    def(globalThis, "Headers", function (init) { var m = {}; this.append = function (k, v) { m[String(k).toLowerCase()] = String(v); }; this.set = this.append; this.get = function (k) { var v = m[String(k).toLowerCase()]; return v === undefined ? null : v; }; this.has = function (k) { return String(k).toLowerCase() in m; }; this.delete = function (k) { delete m[String(k).toLowerCase()]; }; this.forEach = function (cb) { for (var k in m) { cb(m[k], k, this); } }; if (init) { for (var k in init) { if (Object.prototype.hasOwnProperty.call(init, k)) { this.append(k, init[k]); } } } });
  }

  // --- URLSearchParams ---------------------------------------------------------------------
  if (typeof globalThis.URLSearchParams !== "function") {
    def(globalThis, "URLSearchParams", function (init) {
      var pairs = [];
      function add(k, v) { pairs.push([String(k), String(v)]); }
      if (typeof init === "string") {
        var s = init.charAt(0) === "?" ? init.slice(1) : init;
        if (s) {
          var segs = s.split("&");
          for (var i = 0; i < segs.length; i++) {
            if (!segs[i]) { continue; }
            var eq = segs[i].indexOf("=");
            var k = eq < 0 ? segs[i] : segs[i].slice(0, eq);
            var v = eq < 0 ? "" : segs[i].slice(eq + 1);
            try { add(decodeURIComponent(k.replace(/\+/g, " ")), decodeURIComponent(v.replace(/\+/g, " "))); } catch (e) { add(k, v); }
          }
        }
      } else if (init && typeof init === "object") {
        if (typeof init.forEach === "function" && typeof init.length === "number") {
          for (var j = 0; j < init.length; j++) { add(init[j][0], init[j][1]); }
        } else {
          for (var key in init) { if (Object.prototype.hasOwnProperty.call(init, key)) { add(key, init[key]); } }
        }
      }
      this.append = function (k, v) { add(k, v); };
      this.set = function (k, v) { k = String(k); for (var i = pairs.length - 1; i >= 0; i--) { if (pairs[i][0] === k) { pairs.splice(i, 1); } } add(k, v); };
      this.get = function (k) { k = String(k); for (var i = 0; i < pairs.length; i++) { if (pairs[i][0] === k) { return pairs[i][1]; } } return null; };
      this.getAll = function (k) { k = String(k); var out = []; for (var i = 0; i < pairs.length; i++) { if (pairs[i][0] === k) { out.push(pairs[i][1]); } } return out; };
      this.has = function (k) { k = String(k); for (var i = 0; i < pairs.length; i++) { if (pairs[i][0] === k) { return true; } } return false; };
      this.delete = function (k) { k = String(k); for (var i = pairs.length - 1; i >= 0; i--) { if (pairs[i][0] === k) { pairs.splice(i, 1); } } };
      this.forEach = function (cb, thisArg) { for (var i = 0; i < pairs.length; i++) { cb.call(thisArg, pairs[i][1], pairs[i][0], this); } };
      this.keys = function () { return pairs.map(function (p) { return p[0]; }); };
      this.values = function () { return pairs.map(function (p) { return p[1]; }); };
      this.entries = function () { return pairs.slice(); };
      this.toString = function () { return pairs.map(function (p) { return encodeURIComponent(p[0]) + "=" + encodeURIComponent(p[1]); }).join("&"); };
      Object.defineProperty(this, "size", { get: function () { return pairs.length; }, enumerable: false, configurable: true });
    });
  }

  // --- URL ---------------------------------------------------------------------------------
  if (typeof globalThis.URL !== "function") {
    def(globalThis, "URL", function (url, base) {
      var resolved = String(url);
      // Very small relative-resolution against base origin+path.
      if (base != null && !/^[a-zA-Z][a-zA-Z0-9+.\-]*:/.test(resolved)) {
        var b = parseURL(String(base));
        var c0 = resolved.charCodeAt(0);
        if (c0 === 47) { resolved = b.origin + resolved; }            // '/'
        else if (c0 === 63) { resolved = b.origin + b.pathname + resolved; }  // '?'
        else if (c0 === 35) { resolved = b.origin + b.pathname + b.search + resolved; }  // '#'
        else { var dir = b.pathname.replace(/[^/]*$/, ""); resolved = b.origin + dir + resolved; }
      }
      var p = parseURL(resolved);
      this.href = p.href; this.protocol = p.protocol; this.host = p.host; this.hostname = p.hostname;
      this.port = p.port; this.pathname = p.pathname; this.search = p.search; this.hash = p.hash; this.origin = p.origin;
      this.username = ""; this.password = "";
      this.searchParams = new globalThis.URLSearchParams(p.search);
      this.toString = function () { return this.href; }; this.toJSON = function () { return this.href; };
    });
    globalThis.URL.createObjectURL = function () { return "blob:null/0"; };
    globalThis.URL.revokeObjectURL = fn;
  }
  if (typeof globalThis.queueMicrotask !== "function") { /* installed by timers */ }

  // --- misc presence stubs -----------------------------------------------------------------
  def(globalThis, "requestIdleCallback", function (cb) { return setTimeout(function () { try { cb({ didTimeout: false, timeRemaining: function () { return 0; } }); } catch (e) {} }, 1); });
  def(globalThis, "cancelIdleCallback", function (id) { return clearTimeout(id); });

  if (typeof globalThis.structuredClone !== "function") {
    def(globalThis, "structuredClone", function (v) { try { return JSON.parse(JSON.stringify(v)); } catch (e) { return v; } });
  }

  // NodeFilter constants (used with createTreeWalker / createNodeIterator below).
  if (typeof globalThis.NodeFilter === "undefined") {
    def(globalThis, "NodeFilter", {
      FILTER_ACCEPT: 1, FILTER_REJECT: 2, FILTER_SKIP: 3,
      SHOW_ALL: 0xFFFFFFFF, SHOW_ELEMENT: 0x1, SHOW_ATTRIBUTE: 0x2, SHOW_TEXT: 0x4,
      SHOW_CDATA_SECTION: 0x8, SHOW_ENTITY_REFERENCE: 0x10, SHOW_ENTITY: 0x20,
      SHOW_PROCESSING_INSTRUCTION: 0x40, SHOW_COMMENT: 0x80, SHOW_DOCUMENT: 0x100,
      SHOW_DOCUMENT_TYPE: 0x200, SHOW_DOCUMENT_FRAGMENT: 0x400, SHOW_NOTATION: 0x800,
    });
  }

  // createTreeWalker / createNodeIterator — snapshot the accepted descendants of `root` in
  // document order (whatToShow bitmask + optional NodeFilter callback / {acceptNode}); FILTER_REJECT
  // prunes a subtree, FILTER_SKIP / a whatToShow miss skips the node but keeps descending.
  function __makeWalkerNodes(root, whatToShow, filterArg) {
    var mask = (whatToShow === undefined || whatToShow === null) ? 0xFFFFFFFF : (whatToShow >>> 0);
    var filterFn = null;
    if (typeof filterArg === "function") { filterFn = filterArg; }
    else if (filterArg && typeof filterArg.acceptNode === "function") { filterFn = function (n) { return filterArg.acceptNode(n); }; }
    function verdict(n) {
      var t = n.nodeType || 0;
      var shown = t > 0 && (mask & (1 << (t - 1))) !== 0;
      if (!shown) { return 3; }
      if (filterFn) { try { return filterFn(n) || 1; } catch (e) { return 2; } }
      return 1;
    }
    var out = [];
    function visit(n) {
      var v = verdict(n);
      if (v === 2) { return; }
      if (v === 1) { out.push(n); }
      var kids = n.childNodes;
      if (kids) { for (var i = 0; i < kids.length; i++) { visit(kids[i]); } }
    }
    var kids = root && root.childNodes;
    if (kids) { for (var i = 0; i < kids.length; i++) { visit(kids[i]); } }
    return out;
  }
  function __makeTreeWalker(root, whatToShow, filterArg) {
    var nodes = __makeWalkerNodes(root, whatToShow, filterArg);
    var idx = -1;
    var w = { root: root, whatToShow: (whatToShow >>> 0) || 0xFFFFFFFF, filter: filterArg || null, currentNode: root };
    w.nextNode = function () { if (idx + 1 < nodes.length) { idx++; w.currentNode = nodes[idx]; return nodes[idx]; } return null; };
    w.previousNode = function () { if (idx > 0) { idx--; w.currentNode = nodes[idx]; return nodes[idx]; } idx = -1; w.currentNode = root; return null; };
    w.parentNode = function () { var p = w.currentNode && w.currentNode.parentNode; if (p && p !== root) { w.currentNode = p; return p; } return null; };
    w.firstChild = function () { return w.nextNode(); };
    w.lastChild = function () { if (nodes.length) { idx = nodes.length - 1; w.currentNode = nodes[idx]; return nodes[idx]; } return null; };
    w.nextSibling = function () { return null; };
    w.previousSibling = function () { return null; };
    return w;
  }
  function __makeNodeIterator(root, whatToShow, filterArg) {
    var nodes = __makeWalkerNodes(root, whatToShow, filterArg);
    var idx = -1;
    var it = { root: root, whatToShow: (whatToShow >>> 0) || 0xFFFFFFFF, filter: filterArg || null, referenceNode: root, pointerBeforeReferenceNode: true };
    it.nextNode = function () { if (idx + 1 < nodes.length) { idx++; it.referenceNode = nodes[idx]; return nodes[idx]; } return null; };
    it.previousNode = function () { if (idx >= 0) { var n = nodes[idx]; idx--; it.referenceNode = idx >= 0 ? nodes[idx] : root; return n; } return null; };
    it.detach = function () {};
    return it;
  }
  if (typeof globalThis.document !== "undefined" && globalThis.document) {
    if (typeof globalThis.document.createTreeWalker !== "function") {
      def(globalThis.document, "createTreeWalker", function (root, whatToShow, filter) { return __makeTreeWalker(root, whatToShow, filter); });
    }
    if (typeof globalThis.document.createNodeIterator !== "function") {
      def(globalThis.document, "createNodeIterator", function (root, whatToShow, filter) { return __makeNodeIterator(root, whatToShow, filter); });
    }
  }

  // TextEncoder / TextDecoder — UTF-8 only (the common case). Pure JS over Uint8Array.
  if (typeof globalThis.TextEncoder !== "function") {
    def(globalThis, "TextEncoder", function () { this.encoding = "utf-8"; });
    globalThis.TextEncoder.prototype.encode = function (str) {
      str = str === undefined ? "" : String(str);
      var bytes = [];
      for (var i = 0; i < str.length; i++) {
        var c = str.charCodeAt(i);
        if (c < 0x80) { bytes.push(c); }
        else if (c < 0x800) { bytes.push(0xc0 | (c >> 6), 0x80 | (c & 0x3f)); }
        else if (c >= 0xd800 && c <= 0xdbff && i + 1 < str.length) {
          var c2 = str.charCodeAt(++i);
          var cp = 0x10000 + ((c & 0x3ff) << 10) + (c2 & 0x3ff);
          bytes.push(0xf0 | (cp >> 18), 0x80 | ((cp >> 12) & 0x3f), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f));
        } else { bytes.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 0x3f), 0x80 | (c & 0x3f)); }
      }
      return new Uint8Array(bytes);
    };
    globalThis.TextEncoder.prototype.encodeInto = function (str, dest) {
      var enc = this.encode(str), n = Math.min(enc.length, dest.length);
      for (var i = 0; i < n; i++) dest[i] = enc[i];
      return { read: str.length, written: n };
    };
  }
  if (typeof globalThis.TextDecoder !== "function") {
    def(globalThis, "TextDecoder", function (label) { this.encoding = label || "utf-8"; });
    globalThis.TextDecoder.prototype.decode = function (buf) {
      if (!buf) return "";
      var b = buf.buffer ? new Uint8Array(buf.buffer, buf.byteOffset || 0, buf.byteLength) : new Uint8Array(buf);
      var out = "", i = 0;
      while (i < b.length) {
        var c = b[i++];
        if (c < 0x80) { out += String.fromCharCode(c); }
        else if (c < 0xe0) { out += String.fromCharCode(((c & 0x1f) << 6) | (b[i++] & 0x3f)); }
        else if (c < 0xf0) { out += String.fromCharCode(((c & 0x0f) << 12) | ((b[i++] & 0x3f) << 6) | (b[i++] & 0x3f)); }
        else {
          var cp = ((c & 0x07) << 18) | ((b[i++] & 0x3f) << 12) | ((b[i++] & 0x3f) << 6) | (b[i++] & 0x3f);
          cp -= 0x10000;
          out += String.fromCharCode(0xd800 + (cp >> 10), 0xdc00 + (cp & 0x3ff));
        }
      }
      return out;
    };
  }

  // base64 (btoa/atob) — pure JS implementation.
  var B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  def(globalThis, "btoa", function (input) {
    var str = String(input), out = "";
    for (var i = 0; i < str.length;) {
      var c1 = str.charCodeAt(i++) & 0xff;
      var c2 = str.charCodeAt(i++);
      var c3 = str.charCodeAt(i++);
      var e1 = c1 >> 2;
      var e2 = ((c1 & 3) << 4) | ((isNaN(c2) ? 0 : c2) >> 4);
      var e3 = isNaN(c2) ? 64 : (((c2 & 15) << 2) | ((isNaN(c3) ? 0 : c3) >> 6));
      var e4 = isNaN(c3) ? 64 : (c3 & 63);
      out += B64.charAt(e1) + B64.charAt(e2) + (e3 === 64 ? "=" : B64.charAt(e3)) + (e4 === 64 ? "=" : B64.charAt(e4));
    }
    return out;
  });
  def(globalThis, "atob", function (input) {
    // Drop whitespace; keep '=' padding so groups stay 4-aligned.
    var str = String(input).replace(/[^A-Za-z0-9+/=]/g, ""), out = "";
    for (var i = 0; i + 3 < str.length; i += 4) {
      var d1 = B64.indexOf(str.charAt(i));
      var d2 = B64.indexOf(str.charAt(i + 1));
      var p3 = str.charAt(i + 2), p4 = str.charAt(i + 3);
      var d3 = B64.indexOf(p3), d4 = B64.indexOf(p4);
      out += String.fromCharCode(((d1 << 2) | (d2 >> 4)) & 0xff);
      if (p3 !== "=" && d3 >= 0) { out += String.fromCharCode(((d2 & 15) << 4) | (d3 >> 2)); }
      if (p4 !== "=" && d4 >= 0) { out += String.fromCharCode(((d3 & 3) << 6) | d4); }
    }
    return out;
  });

  // crypto: no real RNG available; fill deterministically with a nonzero pattern.
  var cryptoSeed = 0x9e3779b9;
  function nextByte() { cryptoSeed = (cryptoSeed * 1103515245 + 12345) & 0x7fffffff; return ((cryptoSeed >> 16) & 0xff) || 1; }
  globalThis.crypto = {
    getRandomValues: function (arr) { if (arr && typeof arr.length === "number") { for (var i = 0; i < arr.length; i++) { arr[i] = nextByte(); } } return arr; },
    randomUUID: function () {
      var hex = "0123456789abcdef", s = "";
      for (var i = 0; i < 36; i++) {
        if (i === 8 || i === 13 || i === 18 || i === 23) { s += "-"; }
        else if (i === 14) { s += "4"; }
        else if (i === 19) { s += hex.charAt((nextByte() & 0x3) | 0x8); }
        else { s += hex.charAt(nextByte() & 0xf); }
      }
      return s;
    },
    subtle: {}
  };

  // fetch: backed by the native __fetch primitive (synchronous host fetch under the hood; wrapping
  // the result in Promise.resolve is correct for our synchronous drain model). Returns a minimal
  // Response. Rejects with TypeError("Failed to fetch") when the host fetch fails (null body).
  if (typeof globalThis.fetch !== "function") {
    def(globalThis, "fetch", function (input, init) {
      var url;
      try { url = (input && input.url) ? String(input.url) : String(input); }
      catch (e) { url = String(input); }
      var body = (typeof __fetch === "function") ? __fetch(url) : null;
      if (body == null) {
        return Promise.reject(new TypeError("Failed to fetch"));
      }
      var response = {
        ok: true, status: 200, statusText: "OK",
        url: url, redirected: false, type: "basic", bodyUsed: false,
        headers: { get: function () { return null; }, has: function () { return false; },
          forEach: fn, keys: function () { return [][Symbol.iterator](); },
          values: function () { return [][Symbol.iterator](); },
          entries: function () { return [][Symbol.iterator](); } },
        text: function () { return Promise.resolve(body); },
        json: function () { return Promise.resolve(JSON.parse(body)); },
        arrayBuffer: function () { return Promise.resolve(new ArrayBuffer(0)); },
        blob: function () { return Promise.resolve({ size: 0, type: "" }); },
        formData: function () { return Promise.reject(new TypeError("formData not supported")); },
        clone: function () { return response; }
      };
      return Promise.resolve(response);
    });
  }

  // XMLHttpRequest: present but inert.
  def(globalThis, "XMLHttpRequest", function () {
    this.readyState = 0; this.status = 0; this.responseText = ""; this.response = "";
    this.onreadystatechange = null; this.onload = null; this.onerror = null;
    this.open = fn; this.send = fn; this.setRequestHeader = fn; this.abort = fn;
    this.getResponseHeader = function () { return null; }; this.getAllResponseHeaders = function () { return ""; };
    this.addEventListener = fn; this.removeEventListener = fn;
  });

  // Constructors some pages feature-detect / construct.
  if (typeof globalThis.Event !== "function") {
    def(globalThis, "Event", function (type, init) { this.type = String(type); this.bubbles = !!(init && init.bubbles); this.cancelable = !!(init && init.cancelable); this.defaultPrevented = false; this.preventDefault = fn; this.stopPropagation = fn; });
  }
  if (typeof globalThis.CustomEvent !== "function") {
    def(globalThis, "CustomEvent", function (type, init) { this.type = String(type); this.detail = init ? init.detail : null; this.bubbles = !!(init && init.bubbles); this.preventDefault = fn; this.stopPropagation = fn; });
  }
  // Event subclasses (UIEvent/MouseEvent/KeyboardEvent/etc.). Each extends Event with its init
  // fields copied through, so `new MouseEvent('click', {...})` and friends construct without error.
  (function () {
    function makeEventClass(extraDefaults) {
      return function (type, init) {
        init = init || {};
        globalThis.Event.call(this, type, init);
        for (var k in extraDefaults) { this[k] = (k in init) ? init[k] : extraDefaults[k]; }
        this.detail = init.detail || 0;
        this.view = init.view || globalThis.window || null;
      };
    }
    var classes = {
      UIEvent: {},
      FocusEvent: { relatedTarget: null },
      MouseEvent: { screenX: 0, screenY: 0, clientX: 0, clientY: 0, pageX: 0, pageY: 0, button: 0, buttons: 0, ctrlKey: false, shiftKey: false, altKey: false, metaKey: false, relatedTarget: null, movementX: 0, movementY: 0, getModifierState: undefined },
      PointerEvent: { pointerId: 0, width: 1, height: 1, pressure: 0, pointerType: "", isPrimary: false, clientX: 0, clientY: 0, button: 0, buttons: 0 },
      KeyboardEvent: { key: "", code: "", keyCode: 0, which: 0, charCode: 0, location: 0, repeat: false, isComposing: false, ctrlKey: false, shiftKey: false, altKey: false, metaKey: false, getModifierState: undefined },
      WheelEvent: { deltaX: 0, deltaY: 0, deltaZ: 0, deltaMode: 0, clientX: 0, clientY: 0 },
      InputEvent: { data: null, inputType: "", isComposing: false },
      TouchEvent: { touches: [], targetTouches: [], changedTouches: [], ctrlKey: false, shiftKey: false, altKey: false, metaKey: false },
      PopStateEvent: { state: null },
      HashChangeEvent: { oldURL: "", newURL: "" },
      MessageEvent: { data: null, origin: "", lastEventId: "", source: null, ports: [] },
      ProgressEvent: { lengthComputable: false, loaded: 0, total: 0 },
      ErrorEvent: { message: "", filename: "", lineno: 0, colno: 0, error: null },
      AnimationEvent: { animationName: "", elapsedTime: 0, pseudoElement: "" },
      TransitionEvent: { propertyName: "", elapsedTime: 0, pseudoElement: "" },
      CloseEvent: { code: 0, reason: "", wasClean: false },
    };
    for (var name in classes) {
      if (typeof globalThis[name] !== "function") {
        var ctor = makeEventClass(classes[name]);
        ctor.prototype = Object.create(globalThis.Event.prototype || Object.prototype);
        def(globalThis, name, ctor);
      }
    }
    if (typeof globalThis.getModifierState === "undefined") {
      try { globalThis.MouseEvent.prototype.getModifierState = function () { return false; };
            globalThis.KeyboardEvent.prototype.getModifierState = function () { return false; }; } catch (e) {}
    }
  })();

  // --- synthetic event dispatch (driven from Rust on user interaction) ----------------------
  // Build a real bubbling event and walk it up the parent chain (node -> ancestors -> document
  // -> window), invoking each target's __listeners[type] callbacks and its on<type> handler.
  // Returns false if any handler called preventDefault() (caller maps this to "default action
  // should not run"), true otherwise.
  var mouseTypes = { click: 1, mousedown: 1, mouseup: 1, dblclick: 1, contextmenu: 1,
                     pointerdown: 1, pointerup: 1, mouseover: 1, mouseout: 1 };
  def(globalThis, "__dispatchSyntheticEvent", function (nodeId, type, props) {
    var node = null;
    try { node = canon(__wrapNode(nodeId)); } catch (e) { node = null; }
    if (!node) { return true; }
    type = String(type);

    var Ctor = mouseTypes[type] ? globalThis.MouseEvent : globalThis.Event;
    var ev;
    try { ev = new Ctor(type, { bubbles: true, cancelable: true }); }
    catch (e) { ev = { type: type, bubbles: true, cancelable: true, defaultPrevented: false }; }
    // Copy caller-supplied props (clientX/clientY/button/...) onto the event.
    if (props) { for (var k in props) { try { ev[k] = props[k]; } catch (e2) {} } }

    var stopped = false, stoppedImmediate = false;
    ev.defaultPrevented = !!ev.defaultPrevented;
    ev.preventDefault = function () { this.defaultPrevented = true; };
    ev.stopPropagation = function () { stopped = true; };
    ev.stopImmediatePropagation = function () { stopped = true; stoppedImmediate = true; };

    // Build the propagation path: node, its ancestors, document, then window (globalThis).
    var path = [node];
    var cur = node;
    var guard = 0;
    while (cur && guard < 4096) {
      var parent = null;
      try { parent = cur.parentNode; } catch (e3) { parent = null; }
      if (!parent || parent === cur) { break; }
      path.push(parent);
      cur = parent;
      guard++;
    }
    path.push(document);
    path.push(globalThis);

    try { ev.target = node; } catch (e4) {}

    for (var h = 0; h < path.length; h++) {
      if (stopped) { break; }
      var target = path[h];
      if (!target) { continue; }
      try { ev.currentTarget = target; } catch (e5) {}
      var reg = target.__listeners;
      var list = reg ? reg[type] : null;
      if (list) {
        var copy = list.slice();
        for (var i = 0; i < copy.length; i++) {
          try { copy[i].call(target, ev); } catch (e6) { (globalThis.__timerErrors || []).push(String(e6)); }
          if (stoppedImmediate) { break; }
        }
      }
      var on = target["on" + type];
      if (typeof on === "function") {
        try { on.call(target, ev); } catch (e7) { (globalThis.__timerErrors || []).push(String(e7)); }
      }
    }
    return !ev.defaultPrevented;
  });

  // --- non-bubbling synthetic event dispatch ------------------------------------------------
  // Fire `type` on the target node ONLY (no ancestor/document/window propagation). Used for
  // focus/blur, mouseenter/mouseleave which do not bubble. Returns false if preventDefault().
  def(globalThis, "__dispatchSyntheticEventNonBubbling", function (nodeId, type, props) {
    var node = null;
    try { node = canon(__wrapNode(nodeId)); } catch (e) { node = null; }
    if (!node) { return true; }
    type = String(type);

    var Ctor = mouseTypes[type] ? globalThis.MouseEvent : globalThis.Event;
    var ev;
    try { ev = new Ctor(type, { bubbles: false, cancelable: true }); }
    catch (e) { ev = { type: type, bubbles: false, cancelable: true, defaultPrevented: false }; }
    if (props) { for (var k in props) { try { ev[k] = props[k]; } catch (e2) {} } }

    ev.defaultPrevented = !!ev.defaultPrevented;
    ev.preventDefault = function () { this.defaultPrevented = true; };
    ev.stopPropagation = function () {};
    ev.stopImmediatePropagation = function () {};
    try { ev.target = node; ev.currentTarget = node; } catch (e4) {}

    var reg = node.__listeners;
    var list = reg ? reg[type] : null;
    if (list) {
      var copy = list.slice();
      for (var i = 0; i < copy.length; i++) {
        try { copy[i].call(node, ev); } catch (e6) { (globalThis.__timerErrors || []).push(String(e6)); }
      }
    }
    var on = node["on" + type];
    if (typeof on === "function") {
      try { on.call(node, ev); } catch (e7) { (globalThis.__timerErrors || []).push(String(e7)); }
    }
    return !ev.defaultPrevented;
  });

  // mouseover/mouseout bubble; mouseenter/mouseleave do not — register the latter as non-bubbling.
  mouseTypes.mouseenter = 1; mouseTypes.mouseleave = 1; mouseTypes.mousemove = 1;

  // --- checkbox / radio toggle (driven from Rust on click) ----------------------------------
  // Flip a checkbox's `checked`, or set a radio (unchecking same-name siblings), then fire
  // `input` and `change` (both bubbling). The `click` has already been dispatched by the caller.
  // No-op for disabled controls. Returns nothing; the caller reads back the snapshot.
  def(globalThis, "__toggleCheckable", function (nodeId) {
    var el = null;
    try { el = canon(__wrapNode(nodeId)); } catch (e) { el = null; }
    if (!el) { return; }
    var tag = "";
    try { tag = typeof el.tagName === "string" ? el.tagName.toLowerCase() : ""; } catch (e2) {}
    if (tag !== "input") { return; }
    var ty = String(__getAttr(nodeId, "type") || "").toLowerCase();
    if (ty !== "checkbox" && ty !== "radio") { return; }
    if (__getAttr(nodeId, "disabled") != null) { return; }

    if (ty === "checkbox") {
      var on = __getAttr(nodeId, "checked") != null;
      if (on) { __removeAttr(nodeId, "checked"); } else { __setAttr(nodeId, "checked", ""); }
    } else {
      // Radio: uncheck every same-name radio in the same form (or document), then check this one.
      var name = String(__getAttr(nodeId, "name") || "");
      // Find the enclosing <form>, if any.
      var form = null;
      try {
        var c = el;
        while (c) {
          var t = "";
          try { t = typeof c.tagName === "string" ? c.tagName.toLowerCase() : ""; } catch (ef) {}
          if (t === "form") { form = c; break; }
          c = c.parentNode;
        }
      } catch (e3) {}
      var scope = form || document;
      var radios = [];
      try { radios = scope.querySelectorAll("input[type=radio]"); } catch (e4) { radios = []; }
      for (var i = 0; i < radios.length; i++) {
        var r = radios[i];
        var rname = "";
        try { rname = String(r.getAttribute("name") || ""); } catch (e5) {}
        if (rname === name) {
          try { r.removeAttribute("checked"); } catch (e6) {}
        }
      }
      __setAttr(nodeId, "checked", "");
    }
    __dispatchSyntheticEvent(nodeId, "input", {});
    __dispatchSyntheticEvent(nodeId, "change", {});
  });

  // --- key input handler (driven from Rust on physical key presses) -------------------------
  // Fire keydown, mutate the focused text field's value (firing input), then keyup. Returns
  // nothing; the caller reads back the updated DOM snapshot. Text-like <input>/<textarea> only.
  var textInputTypes = { text: 1, search: 1, email: 1, url: 1, tel: 1, password: 1, number: 1, "": 1 };
  def(globalThis, "__handleKeyInput", function (nodeId, key, code) {
    var el = null;
    try { el = canon(__wrapNode(nodeId)); } catch (e) { el = null; }
    if (!el) { return; }
    key = String(key);
    code = String(code);

    // keydown — if defaultPrevented, still send keyup but skip the value mutation.
    var allowMutation = __dispatchSyntheticEvent(nodeId, "keydown", { key: key, code: code });

    if (allowMutation) {
      var tag = "";
      try { tag = typeof el.tagName === "string" ? el.tagName.toLowerCase() : ""; } catch (e2) {}
      var isTextarea = tag === "textarea";
      var isTextInput = false;
      if (tag === "input") {
        var ty = "";
        try { ty = String(__getAttr(nodeId, "type") || "").toLowerCase(); } catch (e3) {}
        isTextInput = !!textInputTypes[ty] || ty === undefined;
      }
      var disabled = false, readonly = false;
      try { disabled = __getAttr(nodeId, "disabled") != null; } catch (e4) {}
      try { readonly = __getAttr(nodeId, "readonly") != null; } catch (e5) {}

      if ((isTextInput || isTextarea) && !disabled && !readonly) {
        var cur = "";
        try { cur = el.value == null ? "" : String(el.value); } catch (e6) { cur = ""; }
        var next = cur;
        var mutated = false;
        if (key === "Backspace") {
          if (cur.length > 0) { next = cur.slice(0, -1); mutated = true; }
          else { mutated = true; }
        } else if (key === "Delete") {
          // Simplified: drop the last char (no caret tracking).
          if (cur.length > 0) { next = cur.slice(0, -1); mutated = true; }
          else { mutated = true; }
        } else if (key === "Enter") {
          if (isTextarea) { next = cur + "\n"; mutated = true; }
          // <input>: Enter submits; no value change here.
        } else if (key.length === 1) {
          next = cur + key; mutated = true;
        }
        if (mutated) {
          try { el.value = next; } catch (e7) {}
          __dispatchSyntheticEvent(nodeId, "input", {});
        }
      }
    }

    // keyup always fires.
    __dispatchSyntheticEvent(nodeId, "keyup", { key: key, code: code });
  });
})();
"#;

// ---------------------------------------------------------------------------------------------
// Event loop drain + script evaluation against a V8 context.
// ---------------------------------------------------------------------------------------------

/// Maximum number of `__runDueTimers()` iterations when draining the event loop.
const EVENT_LOOP_CAP: usize = 10_000;

/// Compile + run a single source string in the current context, capturing console + error.
/// Drains the per-call console buffer of the [`HostState`] into the result. Never panics on a JS
/// error: it is captured into `EvalOutput.error` via a `TryCatch`.
fn eval_source(scope: &mut v8::PinScope, source: &str, name: &str) -> EvalOutput {
    // Clear any leftover console from a prior call so this result only captures its own output.
    if let Some(state) = scope.get_current_context().get_slot::<HostState>() {
        state.console.borrow_mut().clear();
    }

    let result = {
        v8::tc_scope!(let tc, scope);
        let code = match v8::String::new(tc, source) {
            Some(c) => c,
            None => {
                return EvalOutput {
                    value: None,
                    console: Vec::new(),
                    error: Some("source too large for the JS engine".to_string()),
                };
            }
        };
        let resource = v8::String::new(tc, name).unwrap();
        let origin = v8::ScriptOrigin::new(
            tc, resource.into(), 0, 0, false, 0, None, false, false, false, None,
        );
        match v8::Script::compile(tc, code, Some(&origin)) {
            Some(script) => match script.run(tc) {
                Some(value) => {
                    let rendered = if value.is_undefined() {
                        None
                    } else {
                        Some(render_value(tc, value))
                    };
                    Ok(rendered)
                }
                None => Err(format_exception(tc)),
            },
            None => Err(format_exception(tc)),
        }
    };

    // Drain captured console.
    let console = scope
        .get_current_context()
        .get_slot::<HostState>()
        .map(|s| std::mem::take(&mut *s.console.borrow_mut()))
        .unwrap_or_default();

    match result {
        Ok(value) => EvalOutput { value, console, error: None },
        Err(error) => EvalOutput { value: None, console, error: Some(error) },
    }
}

/// Format a caught exception (message + stack) into an error string matching the prior shape.
fn format_exception(tc: &mut v8::PinnedRef<'_, v8::TryCatch<v8::HandleScope>>) -> String {
    if let Some(exception) = tc.exception() {
        // Prefer a stack trace if present; otherwise fall back to the exception's string form.
        if let Some(stack) = tc.stack_trace() {
            let s = stack.to_rust_string_lossy(tc);
            if !s.is_empty() {
                return s;
            }
        }
        return exception.to_rust_string_lossy(tc);
    }
    "uncaught exception".to_string()
}

/// Drive the event loop to completion (or the time/iteration cap) after page sources have run.
/// Fires the DOM lifecycle events, then alternates V8 microtask checkpoints with the JS
/// `__runDueTimers()` driver. Folds any console output + `__timerErrors` produced during the
/// drain into the last result (matching the prior behavior).
/// Returns whether any timer/microtask actually fired (so `tick` can skip a DOM snapshot when
/// nothing happened).
fn drain_event_loop(scope: &mut v8::PinScope, results: &mut [EvalOutput]) -> bool {
    if let Some(state) = scope.get_current_context().get_slot::<HostState>() {
        state.console.borrow_mut().clear();
    }

    // Reset the per-drain interval-fired set, then fire lifecycle events
    // (readystatechange/DOMContentLoaded/load) — idempotent, so re-running on a tick costs nothing.
    eval_internal(
        scope,
        "if (typeof __beginDrain === 'function') { __beginDrain(); } \
         if (typeof __fireLifecycleEvents === 'function') { __fireLifecycleEvents(); }",
        "<lifecycle>",
    );

    let start = std::time::Instant::now();
    let budget = std::time::Duration::from_millis(3000);
    let mut iterations = 0usize;
    let mut did_work = false;
    loop {
        if iterations >= EVENT_LOOP_CAP || start.elapsed() >= budget {
            break;
        }
        // Run any pending V8 microtasks/promise jobs first.
        scope.perform_microtask_checkpoint();

        // Then run one due timer/microtask from the JS event loop.
        let ran = run_due_timers(scope);
        iterations += 1;
        if ran {
            did_work = true;
        } else {
            // Nothing left in the JS loop; one more microtask checkpoint in case the last timer
            // queued a job, then stop if still empty.
            scope.perform_microtask_checkpoint();
            if run_due_timers(scope) {
                did_work = true;
            } else {
                break;
            }
        }
    }

    // Collect timer/microtask errors recorded JS-side.
    let mut extra: Vec<String> = Vec::new();
    if let Some(joined) = eval_to_string(
        scope,
        "(globalThis.__timerErrors || []).join('\\u0000')",
    ) {
        for e in joined.split('\u{0}') {
            if !e.is_empty() {
                extra.push(format!("⚠ {e}"));
            }
        }
    }

    let drained = scope
        .get_current_context()
        .get_slot::<HostState>()
        .map(|s| std::mem::take(&mut *s.console.borrow_mut()))
        .unwrap_or_default();

    if drained.is_empty() && extra.is_empty() {
        return did_work;
    }
    if let Some(last) = results.last_mut() {
        last.console.extend(drained);
        last.console.extend(extra);
    }
    did_work
}

/// Run `globalThis.__runDueTimers()` and return its boolean result (false if absent/empty).
fn run_due_timers(scope: &mut v8::PinScope) -> bool {
    eval_to_bool(scope, "(typeof __runDueTimers === 'function') && __runDueTimers()")
}

/// Evaluate an internal expression, returning its boolean coercion. Errors → false.
fn eval_to_bool(scope: &mut v8::PinScope, source: &str) -> bool {
    v8::tc_scope!(let tc, scope);
    let code = match v8::String::new(tc, source) {
        Some(c) => c,
        None => return false,
    };
    match v8::Script::compile(tc, code, None).and_then(|s| s.run(tc)) {
        Some(v) => v.boolean_value(tc),
        None => false,
    }
}

/// Evaluate an internal expression, returning its string coercion. Errors → None.
fn eval_to_string(scope: &mut v8::PinScope, source: &str) -> Option<String> {
    v8::tc_scope!(let tc, scope);
    let code = v8::String::new(tc, source)?;
    let v = v8::Script::compile(tc, code, None).and_then(|s| s.run(tc))?;
    Some(render_value(tc, v))
}

// ---------------------------------------------------------------------------------------------
// Public API: Runtime, eval_batch, run_with_dom, run_modules.
// ---------------------------------------------------------------------------------------------

/// A JS runtime. Owns one V8 isolate + global context so state persists across `eval` calls.
///
/// The isolate is single-thread-bound, so a `Runtime` must be created and used on the same thread.
pub struct Runtime {
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime {
    /// Build a fresh runtime with `console` + timers installed (no DOM). State persists across
    /// `eval` calls via the owned global context.
    pub fn new() -> Self {
        ensure_v8_initialized();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        let context = {
            v8::scope!(let handle_scope, &mut isolate);
            let context = v8::Context::new(handle_scope, Default::default());
            let scope = &mut v8::ContextScope::new(handle_scope, context);
            // No DOM on this path, but install a HostState so console works (doc is an empty doc).
            let state = HostState::new(Rc::new(RefCell::new(dom::Document::new())));
            scope.get_current_context().set_slot(state);
            let global = scope.get_current_context().global(scope);
            install_console_sink(scope, global);
            eval_internal(scope, TIMERS_BOOTSTRAP, "<timers>");
            v8::Global::new(scope, context)
        };
        Runtime { isolate, context }
    }

    /// Evaluate a script in the owned context. Globals persist across calls. Never panics on a JS
    /// error — it is captured into `EvalOutput.error`.
    pub fn eval(&mut self, source: &str) -> EvalOutput {
        let context = self.context.clone();
        v8::scope!(let handle_scope, &mut self.isolate);
        let local_ctx = v8::Local::new(handle_scope, &context);
        let scope = &mut v8::ContextScope::new(handle_scope, local_ctx);
        eval_source(scope, source, "<eval>")
    }
}

/// Run `sources` in order on a single fresh runtime (so later scripts see earlier globals) and
/// return one [`EvalOutput`] per source.
///
/// Runs on a dedicated worker thread with a generous stack so a runaway script (or deep
/// recursion) can't block or fault the caller; the V8 isolate is created on that worker thread
/// (isolates are single-thread-bound). A panic on the worker is isolated and surfaced as an error.
pub fn eval_batch(sources: Vec<String>) -> Vec<EvalOutput> {
    let count = sources.len();
    let worker = std::thread::Builder::new()
        .name("js-eval".to_string())
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            let mut rt = Runtime::new();
            let mut results: Vec<EvalOutput> = sources.iter().map(|s| rt.eval(s)).collect();
            // Drive the event loop so timers/microtasks the scripts registered actually run.
            let context = rt.context.clone();
            v8::scope!(let handle_scope, &mut rt.isolate);
            let local_ctx = v8::Local::new(handle_scope, &context);
            let scope = &mut v8::ContextScope::new(handle_scope, local_ctx);
            drain_event_loop(scope, &mut results);
            results
        });

    match worker {
        Ok(handle) => handle.join().unwrap_or_else(|_| {
            vec![
                EvalOutput {
                    value: None,
                    console: Vec::new(),
                    error: Some("script execution aborted (panic in JS engine)".to_string()),
                };
                count.max(1)
            ]
        }),
        Err(e) => vec![EvalOutput {
            value: None,
            console: Vec::new(),
            error: Some(format!("could not start JS worker thread: {e}")),
        }],
    }
}

/// Run `sources` in order against the live `doc`, returning the (possibly mutated) document and
/// one [`EvalOutput`] per source.
///
/// The DOM-aware sibling of [`eval_batch`]: the context gets the full browser environment
/// (`window`/`self`/`globalThis`, `location`, a DOM-wired `document`, timers, navigator/etc.) so
/// scripts mutate the real tree and the change is visible in the returned document. Runs on a
/// dedicated worker thread; the V8 isolate, the `Rc<RefCell<Document>>`, and all wrappers live on
/// that thread and never cross the boundary.
pub fn run_with_dom(
    doc: dom::Document,
    sources: Vec<String>,
    url: &str,
) -> (dom::Document, Vec<EvalOutput>) {
    let url = url.to_string();
    // Channel + timeout (like run_modules): heavy classic-script sites (e.g. youtube.com runs
    // hundreds of KB of script) must not block the page load forever. On timeout we render the
    // pre-script DOM rather than hang. `fallback` is that pre-script DOM.
    let (tx, rx) = std::sync::mpsc::channel::<(dom::Document, Vec<EvalOutput>)>();
    let fallback = doc.clone();
    let worker = std::thread::Builder::new()
        .name("js-eval-dom".to_string())
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            let result: (dom::Document, Vec<EvalOutput>) = (move || {
            ensure_v8_initialized();
            let shared: SharedDoc = Rc::new(RefCell::new(doc));
            let mut isolate = v8::Isolate::new(v8::CreateParams::default());
            let mut results: Vec<EvalOutput> = Vec::with_capacity(sources.len());
            {
                v8::scope!(let handle_scope, &mut isolate);
                let context = v8::Context::new(handle_scope, Default::default());
                let scope = &mut v8::ContextScope::new(handle_scope, context);
                let state = HostState::new(Rc::clone(&shared));
                scope.get_current_context().set_slot(state);
                install_browser_environment(scope, &url);

                for source in &sources {
                    results.push(eval_source(scope, source, "<script>"));
                }
                drain_event_loop(scope, &mut results);
            }
            // Recover the owned Document. Dropping the isolate releases the context (and HostState
            // slot, which holds the only other Rc clone of `shared`), so `try_unwrap` succeeds.
            drop(isolate);
            let doc = match Rc::try_unwrap(shared) {
                Ok(cell) => cell.into_inner(),
                Err(rc) => rc.borrow().clone(),
            };
            (doc, results)
            })();
            let _ = tx.send(result);
        });

    match worker {
        Ok(_handle) => {
            // Wait a bounded slice; if scripts don't finish (slow/looping), render the pre-script
            // DOM. The detached worker finishes on its own. A panic drops `tx`, so recv also Errs.
            let budget = std::time::Duration::from_secs(10);
            match rx.recv_timeout(budget) {
                Ok(result) => result,
                Err(_) => (
                    fallback,
                    vec![EvalOutput {
                        value: None,
                        console: Vec::new(),
                        error: Some("script execution timed out or aborted".to_string()),
                    }],
                ),
            }
        }
        Err(e) => (
            fallback,
            vec![EvalOutput {
                value: None,
                console: Vec::new(),
                error: Some(format!("could not start JS worker thread: {e}")),
            }],
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// ES modules + dynamic import (run_modules). V8 handles modules natively; we wire resolution.
// ---------------------------------------------------------------------------------------------

/// Upper bound on the number of distinct modules (static + on-demand) we will ever compile in a
/// single `run_modules` pass. Mirrors the engine's static-graph cap; the on-demand fetcher shares
/// this budget so a runaway dynamic-import chain cannot fetch unboundedly.
const MODULE_CAP: usize = 800;

/// Registry of compiled modules + their (already canonicalized) source map, stored on the context
/// slot so the bare-fn resolve/dynamic-import callbacks can recover it. Keyed by canonical URL.
struct ModuleRegistry {
    /// Canonical URL -> already-rewritten module source. Acts as a warm cache: the engine
    /// pre-fetches the static graph into here, and on-demand fetches are inserted alongside so the
    /// same dynamic module is only fetched once.
    sources: RefCell<HashMap<String, String>>,
    /// Canonical URL -> compiled module. Populated lazily (compile-on-resolve).
    compiled: RefCell<HashMap<String, v8::Global<v8::Module>>>,
    /// `Module::get_identity_hash()` -> the canonical URL it was compiled under. Lets the resolve /
    /// dynamic-import callbacks recover a referrer module's own URL so relative specifiers resolve
    /// against the right base.
    identity_to_url: RefCell<HashMap<i32, String>>,
    /// On-demand fetcher for modules absent from `sources` (dynamic imports of non-pre-fetched
    /// URLs). Called only on the isolate's own worker thread, so blocking inside it is fine.
    /// Shared (via `Rc`) with [`HostState`] so the JS `fetch()` primitive uses the same fetcher.
    fetcher: Rc<dyn Fn(&str) -> Option<String>>,
    /// Page/entry URL, used as the base for resolving specifiers when a referrer's own URL is
    /// unknown (e.g. dynamic `import()` from a non-module classic context).
    base_url: String,
}

impl ModuleRegistry {
    /// Resolve `specifier` against `base` (a canonical URL) via `Url::join`. Returns the canonical
    /// absolute URL, or `specifier` unchanged if neither parses (best-effort, never panics).
    fn resolve_specifier(specifier: &str, base: &str) -> String {
        if let Ok(base_url) = url::Url::parse(base) {
            if let Ok(joined) = base_url.join(specifier) {
                return joined.to_string();
            }
        }
        // Fall back to the specifier itself (already absolute in the common pre-rewritten case).
        url::Url::parse(specifier)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| specifier.to_string())
    }

    /// Obtain the source for a canonical URL: from the warm `sources` cache, or on demand via the
    /// fetcher (which is then cached). Returns None if both miss or the cap is reached.
    fn source_for(&self, url: &str) -> Option<String> {
        if let Some(s) = self.sources.borrow().get(url) {
            return Some(s.clone());
        }
        if self.sources.borrow().len() >= MODULE_CAP {
            return None;
        }
        let fetched = (self.fetcher)(url)?;
        self.sources.borrow_mut().insert(url.to_string(), fetched.clone());
        Some(fetched)
    }
}

/// Compile a module source under its canonical URL origin and register it. Returns the compiled
/// module local, or None on a compile error (the TryCatch holds the exception).
fn compile_and_register<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    url: &str,
    source: &str,
) -> Option<v8::Local<'s, v8::Module>> {
    let registry = scope.get_current_context().get_slot::<ModuleRegistry>()?;
    // Reuse an already-compiled module if present.
    if let Some(g) = registry.compiled.borrow().get(url) {
        return Some(v8::Local::new(scope, g));
    }
    let code = v8::String::new(scope, source)?;
    let resource = v8::String::new(scope, url)?;
    let origin = v8::ScriptOrigin::new(
        scope,
        resource.into(),
        0,
        0,
        false,
        0,
        None,
        false,
        false,
        true, // is_module
        None,
    );
    let mut src = v8::script_compiler::Source::new(code, Some(&origin));
    let module = v8::script_compiler::compile_module(scope, &mut src)?;
    let global = v8::Global::new(scope, module);
    registry.compiled.borrow_mut().insert(url.to_string(), global);
    // Record identity -> URL so the resolve/dynamic-import callbacks can recover this module's own
    // canonical URL when resolving its relative specifiers.
    registry.identity_to_url.borrow_mut().insert(module.get_identity_hash().get() as i32, url.to_string());
    Some(module)
}

/// Get-or-(fetch+compile) the module for a canonical URL: returns an already-compiled module, or
/// fetches its source (warm cache then on-demand fetcher) and compiles it. None on miss/compile
/// error (the latter leaves the exception on the TryCatch).
fn get_or_compile<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    url: &str,
) -> Option<v8::Local<'s, v8::Module>> {
    let registry = scope.get_current_context().get_slot::<ModuleRegistry>()?;
    if let Some(g) = registry.compiled.borrow().get(url) {
        return Some(v8::Local::new(scope, g));
    }
    let source = registry.source_for(url)?;
    compile_and_register(scope, url, &source)
}

/// Resolve a specifier against a referrer module's own URL. Looks the referrer up in the identity
/// map (falling back to the registry's base/page URL), joins the specifier onto it, and returns the
/// canonical absolute URL. Best-effort; never panics.
fn resolve_against_referrer(
    registry: &ModuleRegistry,
    specifier: &str,
    referrer_identity: Option<i32>,
) -> String {
    let base = referrer_identity
        .and_then(|h| registry.identity_to_url.borrow().get(&h).cloned())
        .unwrap_or_else(|| registry.base_url.clone());
    ModuleRegistry::resolve_specifier(specifier, &base)
}

/// Module resolution callback (used during instantiation). We resolve the specifier against the
/// referrer module's canonical URL, then get-or-(fetch+compile) the target. Instantiation recurses,
/// so this transparently loads whole subtrees of dynamically-discovered modules.
fn resolve_module_callback<'s>(
    context: v8::Local<'s, v8::Context>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
    referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    v8::callback_scope!(unsafe scope, context);
    let spec = specifier.to_rust_string_lossy(scope);
    let referrer_identity = referrer.get_identity_hash().get() as i32;
    let url = {
        let registry = scope.get_current_context().get_slot::<ModuleRegistry>()?;
        resolve_against_referrer(&registry, &spec, Some(referrer_identity))
    };
    match get_or_compile(scope, &url) {
        Some(m) => Some(m),
        None => {
            // Surface as a module error rather than panicking: throw so the caught exception
            // propagates to the importing module's instantiation/evaluation.
            let msg = v8::String::new(scope, &format!("module not found: {url}")).unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            None
        }
    }
}

/// Promise reject callback: when a promise is rejected with no handler attached, V8 invokes this.
/// Vue's dev build re-throws errors from inside reactive effects, which surface here as unhandled
/// rejections during the microtask drain. Format the rejection value (its `.stack` if it's an
/// Error, else its string coercion) and push it into the shared console buffer so it reaches the
/// returned `EvalOutput.console`. Never panics.
extern "C" fn promise_reject_callback(msg: v8::PromiseRejectMessage) {
    if msg.get_event() != v8::PromiseRejectEvent::PromiseRejectWithNoHandler {
        return;
    }
    v8::callback_scope!(unsafe scope, &msg);
    let Some(value) = msg.get_value() else { return };
    v8::scope!(let scope, scope);
    // Prefer `.stack` when the rejection is an Error-like object.
    let mut text = render_value(scope, value);
    if value.is_object() {
        if let Ok(obj) = v8::Local::<v8::Object>::try_from(value) {
            if let Some(key) = v8::String::new(scope, "stack") {
                if let Some(stack) = obj.get(scope, key.into()) {
                    if stack.is_string() {
                        text = render_value(scope, stack);
                    }
                }
            }
        }
    }
    if let Some(state) = scope.get_current_context().get_slot::<HostState>() {
        state.console.borrow_mut().push(format!("⚠ Unhandled rejection: {text}"));
    }
}

/// Dynamic `import(specifier)` host callback. The specifier is resolved against the importing
/// module's URL (recovered from the resource name) and then get-or-(fetch+compile)d on demand —
/// this is what unblocks runtime imports of modules NOT in the pre-fetched static graph. We
/// instantiate + evaluate, drain microtasks, and resolve the promise with the namespace; reject on
/// any failure.
fn dynamic_import_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    _host_defined_options: v8::Local<'s, v8::Data>,
    resource_name: v8::Local<'s, v8::Value>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
) -> Option<v8::Local<'s, v8::Promise>> {
    let resolver = v8::PromiseResolver::new(scope)?;
    let promise = resolver.get_promise(scope);
    let spec = specifier.to_rust_string_lossy(scope);

    // The resource name is the importing module's canonical URL (its ScriptOrigin resource). Use it
    // as the base for resolving the requested specifier; fall back to the registry's base URL.
    let resource = if resource_name.is_string() {
        Some(resource_name.to_rust_string_lossy(scope))
    } else {
        None
    };
    let url = match scope.get_current_context().get_slot::<ModuleRegistry>() {
        Some(registry) => {
            let base = resource.unwrap_or_else(|| registry.base_url.clone());
            ModuleRegistry::resolve_specifier(&spec, &base)
        }
        None => {
            let msg = v8::String::new(scope, "no module registry").unwrap();
            let exc = v8::Exception::error(scope, msg);
            resolver.reject(scope, exc);
            return Some(promise);
        }
    };

    // Get-or-(fetch+compile): warm cache, then on-demand fetch for non-pre-fetched URLs.
    let module = {
        v8::tc_scope!(let tc, scope);
        get_or_compile(tc, &url)
    };

    let module = match module {
        Some(m) => m,
        None => {
            let msg = v8::String::new(scope, &format!("dynamic import not found: {url}")).unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            resolver.reject(scope, exc);
            return Some(promise);
        }
    };

    // Instantiate + evaluate (idempotent if already done).
    let ok = {
        v8::tc_scope!(let tc, scope);
        let inst = module.instantiate_module(tc, resolve_module_callback);
        if inst != Some(true) {
            None
        } else {
            let _ = module.evaluate(tc);
            Some(())
        }
    };
    // Drain microtasks so the module's evaluation promise (top-level await) settles before we read
    // its namespace/status.
    scope.perform_microtask_checkpoint();

    match ok {
        Some(()) if module.get_status() != v8::ModuleStatus::Errored => {
            let ns = module.get_module_namespace();
            resolver.resolve(scope, ns);
        }
        _ => {
            let reason = if module.get_status() == v8::ModuleStatus::Errored {
                module.get_exception()
            } else {
                let msg = v8::String::new(scope, &format!("could not instantiate: {url}")).unwrap();
                v8::Exception::type_error(scope, msg)
            };
            resolver.reject(scope, reason);
        }
    }
    Some(promise)
}

/// `import.meta` initialization host callback. V8 calls this lazily the first time a module reads
/// `import.meta`. We populate `import.meta.url` with the module's canonical URL (recovered from the
/// registry's `identity_to_url` map, keyed by `Module::get_identity_hash`), falling back to the
/// page/entry base URL if absent. We also define `import.meta.resolve(spec)` as a small JS closure
/// that resolves a specifier against that URL via the WHATWG `URL` constructor. Best-effort and
/// panic-free: if anything is missing we leave `meta` as V8 created it.
extern "C" fn initialize_import_meta_callback(
    context: v8::Local<v8::Context>,
    module: v8::Local<v8::Module>,
    meta: v8::Local<v8::Object>,
) {
    v8::callback_scope!(unsafe scope, context);
    // Recover the module's canonical URL from the registry, falling back to the base/page URL.
    let url = {
        let Some(registry) = scope.get_current_context().get_slot::<ModuleRegistry>() else {
            return;
        };
        let identity = module.get_identity_hash().get() as i32;
        let mapped = registry.identity_to_url.borrow().get(&identity).cloned();
        mapped.unwrap_or_else(|| registry.base_url.clone())
    };

    // import.meta.url = <canonical url>
    if let (Some(key), Some(val)) =
        (v8::String::new(scope, "url"), v8::String::new(scope, &url))
    {
        meta.create_data_property(scope, key.into(), val.into());
    }

    // import.meta.resolve = (spec) => new URL(spec, <url>).href  (best-effort, never panics).
    let resolve_src = format!(
        "(function(){{const base={base};return function(spec){{return new URL(spec, base).href;}};}})()",
        base = json_string_literal(&url)
    );
    if let Some(code) = v8::String::new(scope, &resolve_src) {
        v8::tc_scope!(let tc, scope);
        if let Some(script) = v8::Script::compile(tc, code, None) {
            if let Some(func) = script.run(tc) {
                if let Some(key) = v8::String::new(tc, "resolve") {
                    meta.create_data_property(tc, key.into(), func);
                }
            }
        }
    }
}

/// Encode `s` as a JSON/JS string literal (double-quoted, with the handful of characters that would
/// break out of a `"..."` literal escaped). Used to embed a URL safely inside generated JS source.
fn json_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
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
    out
}

/// Run the ES module graph for a page. `entries` are the canonical URLs of the entry modules in
/// document order; `modules` maps every canonical module URL to its already-rewritten source.
/// Returns the (possibly mutated) document plus one [`EvalOutput`] per entry. The browser
/// environment is installed identically to [`run_with_dom`], so modules see `document`/`window`.
/// Dynamic `import()` is wired via the isolate's host-import callback.
pub fn run_modules(
    doc: dom::Document,
    url: &str,
    entries: Vec<String>,
    modules: HashMap<String, String>,
    fetcher: Box<dyn Fn(&str) -> Option<String> + Send>,
) -> (dom::Document, Vec<EvalOutput>) {
    let url = url.to_string();
    let (tx, rx) = std::sync::mpsc::channel::<(dom::Document, Vec<EvalOutput>)>();
    let fallback = doc.clone();
    let worker = std::thread::Builder::new()
        .name("js-modules".to_string())
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            ensure_v8_initialized();
            let shared: SharedDoc = Rc::new(RefCell::new(doc));
            let mut isolate = v8::Isolate::new(v8::CreateParams::default());
            isolate.set_host_import_module_dynamically_callback(dynamic_import_callback);
            isolate.set_promise_reject_callback(promise_reject_callback);
            // Populate `import.meta.url` for every module the first time it touches `import.meta`,
            // so relative `new URL(..., import.meta.url)` (e.g. browserscore's support-status.js
            // `fetch(new URL('./support-status.css', import.meta.url))`) resolves correctly.
            isolate.set_host_initialize_import_meta_object_callback(initialize_import_meta_callback);

            let mut results: Vec<EvalOutput> = Vec::with_capacity(entries.len());
            {
                v8::scope!(let handle_scope, &mut isolate);
                let context = v8::Context::new(handle_scope, Default::default());
                let scope = &mut v8::ContextScope::new(handle_scope, context);
                // Share one fetcher between the module loader and the JS `fetch()` primitive.
                let fetcher: Rc<dyn Fn(&str) -> Option<String>> =
                    Rc::new(move |u: &str| fetcher(u));
                let state = HostState::with_fetcher(Rc::clone(&shared), Rc::clone(&fetcher));
                scope.get_current_context().set_slot(state);
                let registry = Rc::new(ModuleRegistry {
                    sources: RefCell::new(modules),
                    compiled: RefCell::new(HashMap::new()),
                    identity_to_url: RefCell::new(HashMap::new()),
                    fetcher,
                    base_url: url.clone(),
                });
                scope.get_current_context().set_slot(registry);
                install_browser_environment(scope, &url);

                // Compile, instantiate, and evaluate each entry module in order.
                for entry in &entries {
                    let outcome = run_one_entry(scope, entry);
                    results.push(outcome);
                }

                drain_event_loop(scope, &mut results);
            }
            drop(isolate);
            let doc = match Rc::try_unwrap(shared) {
                Ok(cell) => cell.into_inner(),
                Err(rc) => rc.borrow().clone(),
            };
            let _ = tx.send((doc, results));
        });

    match worker {
        Ok(_handle) => {
            let budget = std::time::Duration::from_secs(20);
            match rx.recv_timeout(budget) {
                Ok(result) => result,
                Err(_) => (
                    fallback,
                    vec![EvalOutput {
                        value: None,
                        console: Vec::new(),
                        error: Some("module execution timed out or aborted".to_string()),
                    }],
                ),
            }
        }
        Err(e) => (
            fallback,
            vec![EvalOutput {
                value: None,
                console: Vec::new(),
                error: Some(format!("could not start JS worker thread: {e}")),
            }],
        ),
    }
}

/// Compile + instantiate + evaluate a single entry module, returning its [`EvalOutput`] (console
/// captured, error set on any compile/link/evaluate failure).
fn run_one_entry(scope: &mut v8::PinScope, entry: &str) -> EvalOutput {
    if let Some(state) = scope.get_current_context().get_slot::<HostState>() {
        state.console.borrow_mut().clear();
    }

    let error: Option<String> = {
        v8::tc_scope!(let tc, scope);
        let registry = tc.get_current_context().get_slot::<ModuleRegistry>();
        let source = registry.as_ref().and_then(|r| r.source_for(entry));
        match source {
            None => Some(format!("entry module not found: {entry}")),
            Some(src) => match compile_and_register(tc, entry, &src) {
                None => Some(format_exception(tc)),
                Some(module) => {
                    match module.instantiate_module(tc, resolve_module_callback) {
                        Some(true) => {
                            let result = module.evaluate(tc);
                            if module.get_status() == v8::ModuleStatus::Errored {
                                let exc = module.get_exception();
                                Some(render_value(tc, exc))
                            } else if let Some(val) = result {
                                // Top-level-await: if the module returned a rejected promise, surface it.
                                if let Ok(promise) = v8::Local::<v8::Promise>::try_from(val) {
                                    if promise.state() == v8::PromiseState::Rejected {
                                        let reason = promise.result(tc);
                                        Some(render_value(tc, reason))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        }
                        _ => {
                            if tc.has_caught() {
                                Some(format_exception(tc))
                            } else {
                                Some(format!("could not instantiate module: {entry}"))
                            }
                        }
                    }
                }
            },
        }
    };

    let console = scope
        .get_current_context()
        .get_slot::<HostState>()
        .map(|s| std::mem::take(&mut *s.console.borrow_mut()))
        .unwrap_or_default();

    EvalOutput { value: None, console, error }
}

// ---------------------------------------------------------------------------------------------
// Persistent runtime session: keeps the isolate + context alive across operations so a page is
// interactive (event handlers fire, timers keep running) instead of running JS once at load and
// dropping it. Additive to run_with_dom / run_modules — those are unchanged.
// ---------------------------------------------------------------------------------------------

/// Escape a string for embedding inside a double-quoted JS string literal.
fn js_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Commands sent to the session's runtime thread. Each variant that produces a result carries a
/// one-shot reply channel (a fresh `mpsc` per call) so callers block on exactly their own answer.
enum SessionCmd {
    /// Dispatch a synthetic bubbling event to a node, drain the loop, reply with snapshot + console.
    Dispatch {
        node_id: usize,
        kind: String,
        x: f64,
        y: f64,
        reply: std::sync::mpsc::Sender<(dom::Document, Vec<String>)>,
    },
    /// Deliver a key press to a node (keydown → value mutation + input → keyup), drain the loop,
    /// reply with snapshot + console.
    Key {
        node_id: usize,
        key: String,
        code: String,
        reply: std::sync::mpsc::Sender<(dom::Document, Vec<String>)>,
    },
    /// Evaluate an arbitrary JS source string against the persistent context, drain the loop,
    /// reply with snapshot + console. Used for the higher-level interaction helpers (checkbox
    /// toggle, focus/blur/change/submit, hover) that drive bootstrap functions.
    Eval {
        source: String,
        reply: std::sync::mpsc::Sender<(dom::Document, Vec<String>)>,
    },
    /// Run due timers / microtasks; reply `Some(snapshot, console)` if work ran, else `None`.
    Tick {
        reply: std::sync::mpsc::Sender<Option<(dom::Document, Vec<String>)>>,
    },
    /// Stop the loop; the isolate is torn down on the thread it lives on.
    Stop,
}

/// A persistent JS runtime bound to one page. The V8 isolate + context live for the whole session
/// on a dedicated thread; [`dispatch_event`](Session::dispatch_event) and [`tick`](Session::tick)
/// post commands to that thread and block on the reply, returning a fresh DOM snapshot each time.
/// The session keeps mutating the live document; callers render the returned clone.
pub struct Session {
    tx: std::sync::mpsc::Sender<SessionCmd>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    /// Spawn the runtime thread, create the isolate + context, install the browser environment, run
    /// the initial classic `scripts` in order then the module graph (`entries` + `modules`, via
    /// `fetcher`), drain once, and return the session plus the initial DOM snapshot + per-source
    /// [`EvalOutput`]s (one per classic script, then one per module entry — matching the order
    /// `run_with_dom`/`run_modules` would produce).
    pub fn new(
        doc: dom::Document,
        scripts: Vec<String>,
        entries: Vec<String>,
        modules: HashMap<String, String>,
        url: &str,
        fetcher: Box<dyn Fn(&str) -> Option<String> + Send>,
    ) -> (Session, dom::Document, Vec<EvalOutput>) {
        let url = url.to_string();
        let fallback = doc.clone();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<SessionCmd>();
        // One-shot channel for the initial snapshot + per-source outputs.
        let (init_tx, init_rx) = std::sync::mpsc::channel::<(dom::Document, Vec<EvalOutput>)>();

        let spawn = std::thread::Builder::new()
            .name("js-session".to_string())
            .stack_size(256 * 1024 * 1024)
            .spawn(move || {
                // Catch any panic so it never crosses the thread boundary; on panic the init
                // channel is dropped and the caller falls back to an empty snapshot.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    session_thread_main(doc, scripts, entries, modules, url, fetcher, init_tx, cmd_rx);
                }));
            });

        let handle = match spawn {
            Ok(h) => h,
            Err(e) => {
                return (
                    Session { tx: cmd_tx, handle: None },
                    fallback,
                    vec![EvalOutput {
                        value: None,
                        console: Vec::new(),
                        error: Some(format!("could not start JS session thread: {e}")),
                    }],
                );
            }
        };

        // Block (bounded) for the initial load to finish. On timeout/panic, render the pre-script
        // DOM — matching the existing channel-timeout fallback in run_with_dom/run_modules.
        let budget = std::time::Duration::from_secs(20);
        let (snapshot, outputs) = match init_rx.recv_timeout(budget) {
            Ok(result) => result,
            Err(_) => (
                fallback,
                vec![EvalOutput {
                    value: None,
                    console: Vec::new(),
                    error: Some("session load timed out or aborted".to_string()),
                }],
            ),
        };

        (Session { tx: cmd_tx, handle: Some(handle) }, snapshot, outputs)
    }

    /// Dispatch a synthetic bubbling event to `node_id`, drain the event loop, and return a fresh
    /// DOM snapshot + the console lines produced during this operation. Synchronous (blocks on the
    /// reply): callers invoke this from their load/UI thread. Returns an empty snapshot/console if
    /// the session thread is gone.
    pub fn dispatch_event(
        &self,
        node_id: usize,
        kind: &str,
        client_x: f64,
        client_y: f64,
    ) -> (dom::Document, Vec<String>) {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<(dom::Document, Vec<String>)>();
        let cmd = SessionCmd::Dispatch {
            node_id,
            kind: kind.to_string(),
            x: client_x,
            y: client_y,
            reply: reply_tx,
        };
        if self.tx.send(cmd).is_err() {
            return (dom::Document::new(), Vec::new());
        }
        reply_rx.recv().unwrap_or_else(|_| (dom::Document::new(), Vec::new()))
    }

    /// Deliver a key press to `node_id`: fires `keydown`, mutates the focused text field's value
    /// (firing `input`) unless `keydown` was default-prevented, then fires `keyup`. Drains the
    /// event loop and returns a fresh DOM snapshot + console. Synchronous (blocks on the reply).
    pub fn dispatch_key(
        &self,
        node_id: usize,
        key: &str,
        code: &str,
    ) -> (dom::Document, Vec<String>) {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<(dom::Document, Vec<String>)>();
        let cmd = SessionCmd::Key {
            node_id,
            key: key.to_string(),
            code: code.to_string(),
            reply: reply_tx,
        };
        if self.tx.send(cmd).is_err() {
            return (dom::Document::new(), Vec::new());
        }
        reply_rx.recv().unwrap_or_else(|_| (dom::Document::new(), Vec::new()))
    }

    /// Evaluate an arbitrary JS source string against the live context, drain the event loop, and
    /// return a fresh DOM snapshot + console. Backs the higher-level interaction helpers below.
    fn eval_interact(&self, source: String) -> (dom::Document, Vec<String>) {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<(dom::Document, Vec<String>)>();
        if self.tx.send(SessionCmd::Eval { source, reply: reply_tx }).is_err() {
            return (dom::Document::new(), Vec::new());
        }
        reply_rx.recv().unwrap_or_else(|_| (dom::Document::new(), Vec::new()))
    }

    /// Toggle a checkbox / radio `node_id`: flips a checkbox's `checked`, or sets a radio
    /// (unchecking same-`name` siblings in the same form/document), then fires bubbling `input`
    /// and `change` events. No-op for disabled / non-checkable controls. The caller is expected to
    /// have already fired `click`. Returns a fresh DOM snapshot + console.
    pub fn toggle_checkbox(&self, node_id: usize) -> (dom::Document, Vec<String>) {
        self.eval_interact(format!("__toggleCheckable({node_id})"))
    }

    /// Fire a synthetic **bubbling** event of `kind` on `node_id` (empty props), drain the loop,
    /// and return a fresh DOM snapshot + console. Used for `change`/`submit`.
    pub fn fire_event(&self, node_id: usize, kind: &str) -> (dom::Document, Vec<String>) {
        self.eval_interact(format!(
            "__dispatchSyntheticEvent({}, {}, {{}})",
            node_id,
            js_string_literal(kind)
        ))
    }

    /// Fire a synthetic **non-bubbling** event of `kind` on `node_id` (target only), drain the
    /// loop, and return a fresh DOM snapshot + console. Used for `focus`/`blur`/`mouseenter`/
    /// `mouseleave`.
    pub fn fire_event_nonbubbling(&self, node_id: usize, kind: &str) -> (dom::Document, Vec<String>) {
        self.eval_interact(format!(
            "__dispatchSyntheticEventNonBubbling({}, {}, {{}})",
            node_id,
            js_string_literal(kind)
        ))
    }

    /// Run due timers / microtasks (e.g. for animations or deferred work) and return a fresh DOM
    /// snapshot + console. Synchronous; empty snapshot/console if the session thread is gone.
    /// Run any due timers/microtasks. Returns the updated DOM snapshot + console ONLY if work
    /// actually ran (so an idle tick is cheap — no DOM clone, no re-render). `None` = nothing due.
    pub fn tick(&self) -> Option<(dom::Document, Vec<String>)> {
        let (reply_tx, reply_rx) =
            std::sync::mpsc::channel::<Option<(dom::Document, Vec<String>)>>();
        if self.tx.send(SessionCmd::Tick { reply: reply_tx }).is_err() {
            return None;
        }
        reply_rx.recv().unwrap_or(None)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Ask the runtime thread to stop, then join so the isolate is dropped on its own thread.
        let _ = self.tx.send(SessionCmd::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Body of the session runtime thread: owns the isolate + persistent context for the whole session.
#[allow(clippy::too_many_arguments)]
fn session_thread_main(
    doc: dom::Document,
    scripts: Vec<String>,
    entries: Vec<String>,
    modules: HashMap<String, String>,
    url: String,
    fetcher: Box<dyn Fn(&str) -> Option<String> + Send>,
    init_tx: std::sync::mpsc::Sender<(dom::Document, Vec<EvalOutput>)>,
    cmd_rx: std::sync::mpsc::Receiver<SessionCmd>,
) {
    ensure_v8_initialized();
    let shared: SharedDoc = Rc::new(RefCell::new(doc));
    // Keep the isolate owned by this thread for the whole session.
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    // Register the same isolate-level callbacks run_modules uses so modules + dynamic import work.
    isolate.set_host_import_module_dynamically_callback(dynamic_import_callback);
    isolate.set_promise_reject_callback(promise_reject_callback);
    isolate.set_host_initialize_import_meta_object_callback(initialize_import_meta_callback);

    // Create the context once and persist it as a Global across all operations.
    let context: v8::Global<v8::Context> = {
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        // Share one fetcher between the module loader and the JS `fetch()` primitive (as run_modules).
        let fetcher: Rc<dyn Fn(&str) -> Option<String>> = Rc::new(move |u: &str| fetcher(u));
        let state = HostState::with_fetcher(Rc::clone(&shared), Rc::clone(&fetcher));
        scope.get_current_context().set_slot(state);
        let registry = Rc::new(ModuleRegistry {
            sources: RefCell::new(modules),
            compiled: RefCell::new(HashMap::new()),
            identity_to_url: RefCell::new(HashMap::new()),
            fetcher,
            base_url: url.clone(),
        });
        scope.get_current_context().set_slot(registry);
        install_browser_environment(scope, &url);

        // Run initial classic scripts in order, then the module graph, exactly as the load path.
        let mut results: Vec<EvalOutput> =
            Vec::with_capacity(scripts.len() + entries.len());
        for source in &scripts {
            results.push(eval_source(scope, source, "<script>"));
        }
        for entry in &entries {
            results.push(run_one_entry(scope, entry));
        }
        drain_event_loop(scope, &mut results);
        // Load drain done; switch the timer clock to real time so subsequent ticks/events run
        // setInterval/setTimeout/rAF over actual elapsed time.
        eval_internal(scope, "if (typeof __enterRealtime === 'function') { __enterRealtime(); }", "<realtime>");

        // Send the initial snapshot back to Session::new's caller.
        let _ = init_tx.send((shared.borrow().clone(), results));
        v8::Global::new(scope, context)
    };

    // Command loop: each op re-enters the persistent context via Local::new(global).
    for cmd in cmd_rx {
        match cmd {
            SessionCmd::Dispatch { node_id, kind, x, y, reply } => {
                let ctx = context.clone();
                v8::scope!(let handle_scope, &mut isolate);
                let local_ctx = v8::Local::new(handle_scope, &ctx);
                let scope = &mut v8::ContextScope::new(handle_scope, local_ctx);
                let source = format!(
                    "__dispatchSyntheticEvent({}, {}, {{clientX:{}, clientY:{}, button:0}})",
                    node_id,
                    js_string_literal(&kind),
                    x,
                    y,
                );
                // Run the dispatch as one op, then drain the loop, folding console into a result.
                let mut results = vec![eval_source(scope, &source, "<dispatch>")];
                drain_event_loop(scope, &mut results);
                let console = results.into_iter().flat_map(|r| r.console).collect();
                let _ = reply.send((shared.borrow().clone(), console));
            }
            SessionCmd::Key { node_id, key, code, reply } => {
                let ctx = context.clone();
                v8::scope!(let handle_scope, &mut isolate);
                let local_ctx = v8::Local::new(handle_scope, &ctx);
                let scope = &mut v8::ContextScope::new(handle_scope, local_ctx);
                let source = format!(
                    "__handleKeyInput({}, {}, {})",
                    node_id,
                    js_string_literal(&key),
                    js_string_literal(&code),
                );
                let mut results = vec![eval_source(scope, &source, "<key>")];
                drain_event_loop(scope, &mut results);
                let console = results.into_iter().flat_map(|r| r.console).collect();
                let _ = reply.send((shared.borrow().clone(), console));
            }
            SessionCmd::Eval { source, reply } => {
                let ctx = context.clone();
                v8::scope!(let handle_scope, &mut isolate);
                let local_ctx = v8::Local::new(handle_scope, &ctx);
                let scope = &mut v8::ContextScope::new(handle_scope, local_ctx);
                let mut results = vec![eval_source(scope, &source, "<interact>")];
                drain_event_loop(scope, &mut results);
                let console = results.into_iter().flat_map(|r| r.console).collect();
                let _ = reply.send((shared.borrow().clone(), console));
            }
            SessionCmd::Tick { reply } => {
                let ctx = context.clone();
                v8::scope!(let handle_scope, &mut isolate);
                let local_ctx = v8::Local::new(handle_scope, &ctx);
                let scope = &mut v8::ContextScope::new(handle_scope, local_ctx);
                let mut results = vec![EvalOutput::default()];
                let did_work = drain_event_loop(scope, &mut results);
                // Only snapshot+report when something actually ran, so idle ticks are cheap.
                if did_work {
                    let console = results.into_iter().flat_map(|r| r.console).collect();
                    let _ = reply.send(Some((shared.borrow().clone(), console)));
                } else {
                    let _ = reply.send(None);
                }
            }
            SessionCmd::Stop => break,
        }
    }
    // Loop ended (Stop or sender dropped). Drop the isolate on its own thread.
    drop(context);
    drop(isolate);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_returns_value_string() {
        let mut rt = Runtime::new();
        let out = rt.eval("1 + 2 * 3");
        assert_eq!(out.value.as_deref(), Some("7"));
        assert!(out.error.is_none());
        assert!(out.console.is_empty());
    }

    #[test]
    fn console_log_is_captured() {
        let mut rt = Runtime::new();
        let out = rt.eval(r#"console.log("a", 1 + 1)"#);
        assert!(out.error.is_none(), "unexpected error: {:?}", out.error);
        assert_eq!(out.console, vec!["a 2".to_string()]);
    }

    #[test]
    fn console_handles_multiple_calls_and_types() {
        let mut rt = Runtime::new();
        let out = rt.eval(r#"console.log("x"); console.warn(true, [1,2,3]);"#);
        assert!(out.error.is_none());
        assert_eq!(out.console.len(), 2);
        assert_eq!(out.console[0], "x");
        // boolean + array formatting
        assert!(out.console[1].contains("true"));
        assert!(out.console[1].contains("1,2,3"));
    }

    #[test]
    fn syntax_error_populates_error_without_panic() {
        let mut rt = Runtime::new();
        let out = rt.eval("function (");
        assert!(out.error.is_some());
        assert!(out.value.is_none());
    }

    #[test]
    fn thrown_error_populates_error() {
        let mut rt = Runtime::new();
        let out = rt.eval(r#"throw new Error("boom")"#);
        assert!(out.error.is_some());
        assert!(out.error.as_deref().unwrap().contains("boom"));
    }

    #[test]
    fn deeply_nested_input_does_not_overflow() {
        // Regression: a recursive-descent parser can overflow a small thread stack on
        // deeply-nested real-world JS (e.g. youtube.com). `eval_batch` runs on a large stack and
        // V8 caps its own stack depth, so this must not crash the process — it either parses or
        // errors (V8 reports "Maximum call stack size exceeded"), but never faults.
        let depth = 4_000;
        let src = format!("{}1{}", "(".repeat(depth), ")".repeat(depth));
        let out = eval_batch(vec![src]);
        assert_eq!(out.len(), 1);
        assert!(out[0].error.is_some() || out[0].value.as_deref() == Some("1"));
    }

    #[test]
    fn eval_batch_shares_globals_in_order() {
        let out = eval_batch(vec!["var n = 21;".to_string(), "n * 2".to_string()]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].value.as_deref(), Some("42"));
    }

    #[test]
    fn state_persists_across_evals() {
        let mut rt = Runtime::new();
        let first = rt.eval("var x = 5;");
        assert!(first.error.is_none());
        let second = rt.eval("x * 2");
        assert_eq!(second.value.as_deref(), Some("10"));
    }

    // --- DOM-aware path (`run_with_dom`) ------------------------------------------------

    /// Build `<html><head><title>..</title></head><body>..</body></html>` plus any extra
    /// body children, returning the doc and the body id.
    fn doc_with_body(title: &str) -> (dom::Document, dom::NodeId) {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let html = doc.append_element(root, "html");
        let head = doc.append_element(html, "head");
        let t = doc.append_element(head, "title");
        doc.append_child(t, dom::NodeData::Text(title.to_string()));
        let body = doc.append_element(html, "body");
        (doc, body)
    }

    #[test]
    fn inner_html_serializes_child_markup_not_just_text() {
        // innerHTML must return tags + attributes (so framework in-DOM templates survive), not a
        // flattened text run.
        let (mut doc, body) = doc_with_body("");
        let span = doc.append_element(body, "span");
        if let dom::NodeData::Element(e) = &mut doc.get_mut(span).data {
            e.attrs.insert("class".to_string(), "hi".to_string());
        }
        doc.append_child(span, dom::NodeData::Text("x".to_string()));
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"document.body.innerHTML"#.to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some(r#"<span class="hi">x</span>"#));
    }

    #[test]
    fn window_self_globalthis_are_aliased_objects() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                "typeof window === 'object' && typeof self === 'object' && window === self && window === globalThis"
                    .to_string(),
                "window.foo = 123; foo".to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("true"));
        // Setting a property on `window` creates a global.
        assert_eq!(out[1].value.as_deref(), Some("123"));
    }

    #[test]
    fn get_element_by_id_text_content_mutation_is_visible() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let html = doc.append_element(root, "html");
        let body = doc.append_element(html, "body");
        let p = doc.append_element(body, "p");
        doc.get_mut(p).data = match doc.get(p).data.clone() {
            dom::NodeData::Element(mut e) => {
                e.attrs.insert("id".to_string(), "t".to_string());
                dom::NodeData::Element(e)
            }
            other => other,
        };
        doc.append_child(p, dom::NodeData::Text("old".to_string()));

        let (doc, out) = run_with_dom(
            doc,
            vec![r#"document.getElementById("t").textContent = "new""#.to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(text_content(&doc, p), "new");
    }

    #[test]
    fn document_title_returns_title_text() {
        let (doc, _) = doc_with_body("My Page");
        let (_doc, out) = run_with_dom(doc, vec!["document.title".to_string()], "https://example.com/");
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("My Page"));
    }

    #[test]
    fn create_element_and_append_child_shows_up_in_document() {
        let (doc, body) = doc_with_body("");
        let (doc, out) = run_with_dom(
            doc,
            vec![
                r#"var el = document.createElement("span"); el.textContent = "hi"; document.body.appendChild(el);"#
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        // The body now has a span child whose text is "hi".
        let children = &doc.get(body).children;
        assert_eq!(children.len(), 1, "expected one child under body");
        let span = children[0];
        match &doc.get(span).data {
            dom::NodeData::Element(e) => assert_eq!(e.tag, "span"),
            other => panic!("expected span element, got {other:?}"),
        }
        assert_eq!(text_content(&doc, span), "hi");
    }

    #[test]
    fn inner_html_setter_parses_into_real_child_nodes() {
        // Vue's template compiler relies on this: assigning markup must build navigable
        // element/text nodes, not a single flattened Text node.
        let (doc, _body) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                r#"var el = document.createElement("div");
                   el.innerHTML = '<div foo="bar">hi</div>';
                   [el.children.length,
                    el.children[0].tagName,
                    el.children[0].getAttribute("foo"),
                    el.children[0].textContent].join("|")"#
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("1|DIV|bar|hi"));
    }

    // --- Timer / event-loop APIs --------------------------------------------------------

    #[test]
    fn timer_apis_are_defined() {
        let mut rt = Runtime::new();
        let out = rt.eval(
            "[typeof setTimeout, typeof setInterval, typeof clearTimeout, typeof clearInterval, \
             typeof queueMicrotask, typeof requestAnimationFrame, typeof cancelAnimationFrame].join(',')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(
            out.value.as_deref(),
            Some("function,function,function,function,function,function,function")
        );
    }

    #[test]
    fn set_timeout_callback_runs_and_logs() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) =
            run_with_dom(doc, vec![r#"setTimeout(() => console.log("tick"), 0);"#.to_string()], "https://example.com/");
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        let all: Vec<String> = out.iter().flat_map(|o| o.console.clone()).collect();
        assert!(all.iter().any(|l| l == "tick"), "expected 'tick' in {all:?}");
    }

    #[test]
    fn timers_run_in_delay_order() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"setTimeout(() => console.log("slow"), 50); setTimeout(() => console.log("fast"), 10);"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        let all: Vec<String> = out.iter().flat_map(|o| o.console.clone()).collect();
        let fast = all.iter().position(|l| l == "fast");
        let slow = all.iter().position(|l| l == "slow");
        assert!(fast.is_some() && slow.is_some(), "got {all:?}");
        assert!(fast < slow, "fast (10ms) must run before slow (50ms): {all:?}");
    }

    #[test]
    fn clear_timeout_cancels_callback() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var id = setTimeout(() => console.log("nope"), 0); clearTimeout(id);"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        let all: Vec<String> = out.iter().flat_map(|o| o.console.clone()).collect();
        assert!(!all.iter().any(|l| l == "nope"), "cancelled callback ran: {all:?}");
    }

    #[test]
    fn set_interval_is_bounded_and_does_not_hang() {
        // A repeating timer fires AT MOST ONCE per drain — so even a self-perpetuating interval
        // can never spin or hang during a load (it continues over real time via Engine::tick;
        // see `session_timer_runs_on_tick`). One-shots and rAF still run freely.
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"globalThis.n = 0; setInterval(() => { globalThis.n++; console.log("tick" + globalThis.n); }, 1);"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        let all: Vec<String> = out.iter().flat_map(|o| o.console.clone()).collect();
        assert_eq!(
            all.iter().filter(|l| l.starts_with("tick")).count(),
            1,
            "interval should fire exactly once per load drain: {all:?}"
        );
        assert!(all.iter().any(|l| l == "tick1"), "interval should fire once: {all:?}");
    }

    #[test]
    fn queue_microtask_runs_before_timers() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"setTimeout(() => console.log("timer"), 0); queueMicrotask(() => console.log("micro"));"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        let all: Vec<String> = out.iter().flat_map(|o| o.console.clone()).collect();
        let micro = all.iter().position(|l| l == "micro");
        let timer = all.iter().position(|l| l == "timer");
        assert!(micro.is_some() && timer.is_some(), "got {all:?}");
        assert!(micro < timer, "microtask must run before timer: {all:?}");
    }

    #[test]
    fn throwing_timer_does_not_kill_loop_and_is_reported() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"setTimeout(() => { throw new Error("boom"); }, 0); setTimeout(() => console.log("after"), 5);"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        let all: Vec<String> = out.iter().flat_map(|o| o.console.clone()).collect();
        // The later timer still ran despite the earlier one throwing.
        assert!(all.iter().any(|l| l == "after"), "loop died on throw: {all:?}");
        // The error surfaced (prefixed with the warning marker).
        assert!(all.iter().any(|l| l.contains('⚠') && l.contains("boom")), "error not reported: {all:?}");
    }

    #[test]
    fn request_animation_frame_runs_and_cancel_works() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"requestAnimationFrame(() => console.log("raf")); var c = requestAnimationFrame(() => console.log("cancelled")); cancelAnimationFrame(c);"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        let all: Vec<String> = out.iter().flat_map(|o| o.console.clone()).collect();
        assert!(all.iter().any(|l| l == "raf"), "rAF did not run: {all:?}");
        assert!(!all.iter().any(|l| l == "cancelled"), "cancelAnimationFrame failed: {all:?}");
    }

    #[test]
    fn set_attribute_and_class_name_round_trip() {
        let (doc, _body) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                r#"var b = document.body; b.setAttribute("data-x", "y"); b.className = "a b"; b.getAttribute("data-x") + "|" + b.className"#
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("y|a b"));
    }

    // --- Browser environment (`install_browser_env`) ------------------------------------

    /// Convenience: run one expression source against a fresh doc+body at the given URL and
    /// return its [`EvalOutput`].
    fn env_eval(url: &str, src: &str) -> EvalOutput {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(doc, vec![src.to_string()], url);
        out.into_iter().next().unwrap()
    }

    #[test]
    fn navigator_is_a_real_enumerable_object() {
        let out = env_eval(
            "https://example.com/foo?q=1#h",
            "typeof navigator === 'object' && Object.keys(navigator).length > 0 \
             && typeof navigator.userAgent === 'string' && navigator.userAgent.length > 0",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("true"));
    }

    #[test]
    fn object_keys_and_assign_do_not_throw_on_apis() {
        // The original google.com failure: feature-detection runs Object.keys/Object.assign over
        // navigator / matchMedia results. These must succeed without throwing.
        let out = env_eval(
            "https://example.com/",
            "var a = Object.assign({}, navigator); var b = Object.keys(matchMedia('x')); \
             var c = Object.assign({}, getComputedStyle(document.body)); 'ok'",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("ok"));
    }

    #[test]
    fn location_is_parsed_from_url() {
        let out = env_eval(
            "https://example.com/foo?q=1#h",
            "[location.hostname, location.pathname, location.search, location.hash, location.protocol, location.origin].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(
            out.value.as_deref(),
            Some("example.com|/foo|?q=1|#h|https:|https://example.com")
        );
    }

    #[test]
    fn location_with_port_and_no_path() {
        let out = env_eval(
            "http://localhost:8080",
            "[location.hostname, location.port, location.pathname, location.protocol].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("localhost|8080|/|http:"));
    }

    #[test]
    fn local_storage_round_trips_and_tracks_length() {
        let out = env_eval(
            "https://example.com/",
            "localStorage.setItem('k', 'v'); var got = localStorage.getItem('k'); \
             got + '|' + localStorage.length",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("v|1"));
    }

    #[test]
    fn match_media_returns_non_matching_list() {
        let out = env_eval(
            "https://example.com/",
            "matchMedia('(max-width: 600px)').matches === false",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("true"));
    }

    #[test]
    fn get_computed_style_returns_empty_strings() {
        let out = env_eval(
            "https://example.com/",
            "getComputedStyle(document.body).getPropertyValue('color') === '' \
             && getComputedStyle(document.body).color === ''",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("true"));
    }

    #[test]
    fn add_event_listener_exists_on_window_and_document() {
        let out = env_eval(
            "https://example.com/",
            "[typeof window.addEventListener, typeof document.addEventListener, \
              typeof window.dispatchEvent, typeof document.removeEventListener].join(',')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("function,function,function,function"));
    }

    #[test]
    fn dom_content_loaded_listener_fires_during_drain() {
        // A DOMContentLoaded handler registered by a page script must actually run (lifecycle
        // dispatch in the drain). We observe it via captured console output.
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"document.addEventListener("DOMContentLoaded", function () { console.log("dcl-fired"); });
                    window.addEventListener("load", function () { console.log("load-fired"); });"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        let all: Vec<String> = out.iter().flat_map(|o| o.console.clone()).collect();
        assert!(all.iter().any(|l| l == "dcl-fired"), "DOMContentLoaded did not fire: {all:?}");
        assert!(all.iter().any(|l| l == "load-fired"), "load did not fire: {all:?}");
    }

    #[test]
    fn fetch_and_xhr_are_present() {
        let out = env_eval(
            "https://example.com/",
            "[typeof fetch, typeof XMLHttpRequest, typeof btoa, typeof atob, \
              typeof structuredClone, typeof requestIdleCallback].join(',')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(
            out.value.as_deref(),
            Some("function,function,function,function,function,function")
        );
    }

    #[test]
    fn btoa_atob_round_trip() {
        let out = env_eval("https://example.com/", "atob(btoa('hello world'))");
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("hello world"));
    }

    #[test]
    fn crypto_get_random_values_fills_nonzero() {
        let out = env_eval(
            "https://example.com/",
            "var a = new Uint8Array(4); crypto.getRandomValues(a); \
             a.every(function (x) { return x !== 0; })",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("true"));
    }

    #[test]
    fn created_element_style_and_class_list_do_not_throw() {
        let out = env_eval(
            "https://example.com/",
            "var el = document.createElement('div'); el.style.color = 'red'; \
             el.classList.add('a'); el.classList.add('b'); \
             el.dataset.x = '1'; \
             el.style.color + '|' + el.classList.contains('a') + '|' + el.className",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("red|true|a b"));
    }

    #[test]
    fn document_cookie_get_set_round_trips() {
        let out = env_eval(
            "https://example.com/",
            "document.cookie = 'a=1; Path=/'; document.cookie = 'b=2'; document.cookie",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("a=1; b=2"));
    }

    #[test]
    fn document_url_fields_populated() {
        let out = env_eval(
            "https://example.com/foo?q=1#h",
            "[document.URL, document.domain, document.referrer].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(
            out.value.as_deref(),
            Some("https://example.com/foo?q=1#h|example.com|")
        );
    }

    /// Read an element's raw attribute from the DOM (helper for write-through assertions).
    fn attr_of(doc: &dom::Document, id: dom::NodeId, name: &str) -> Option<String> {
        match &doc.get(id).data {
            dom::NodeData::Element(e) => e.attrs.get(name).cloned(),
            _ => None,
        }
    }

    #[test]
    fn style_assignment_writes_through_to_dom_style_attr() {
        let (doc, body) = doc_with_body("");
        let (doc, out) = run_with_dom(
            doc,
            vec![r#"document.body.style.display = "none"; document.body.style.display"#.to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        // Reading back returns the value just set.
        assert_eq!(out[0].value.as_deref(), Some("none"));
        // And the change is written through to the real DOM `style` attribute.
        let style = attr_of(&doc, body, "style").unwrap_or_default();
        assert!(style.contains("display: none"), "style attr was {style:?}");
    }

    #[test]
    fn style_camel_case_maps_to_kebab_and_persists() {
        let (doc, body) = doc_with_body("");
        let (doc, out) = run_with_dom(
            doc,
            vec![r#"var b = document.body; b.style.backgroundColor = "red"; b.style.backgroundColor"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("red"));
        let style = attr_of(&doc, body, "style").unwrap_or_default();
        assert!(style.contains("background-color: red"), "style attr was {style:?}");
    }

    #[test]
    fn style_reads_existing_style_attribute() {
        // Pre-seed a style="" attribute and confirm el.style reads from it.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let html = doc.append_element(root, "html");
        let body = doc.append_element(html, "body");
        if let dom::NodeData::Element(e) = &mut doc.get_mut(body).data {
            e.attrs.insert("style".into(), "display: none; color: blue".into());
        }
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"document.body.style.display + "|" + document.body.style.color"#.to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("none|blue"));
    }

    #[test]
    fn class_list_add_remove_writes_through_to_class_attr() {
        let (doc, body) = doc_with_body("");
        let (doc, out) = run_with_dom(
            doc,
            vec![r#"var b = document.body; b.classList.add("a"); b.classList.add("b"); b.classList.add("a"); b.classList.remove("b"); b.classList.contains("a") + "|" + b.className"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("true|a"));
        // The DOM `class` attribute reflects the change (dedup, removal applied).
        assert_eq!(attr_of(&doc, body, "class").as_deref(), Some("a"));
    }

    #[test]
    fn set_attribute_class_and_classlist_are_consistent() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var b = document.body; b.setAttribute("class", "x y"); b.classList.contains("y") + "|" + b.classList.length"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("true|2"));
    }

    #[test]
    fn query_selector_all_returns_multiple() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let html = doc.append_element(root, "html");
        let body = doc.append_element(html, "body");
        for _ in 0..3 {
            let p = doc.append_element(body, "p");
            if let dom::NodeData::Element(e) = &mut doc.get_mut(p).data {
                e.attrs.insert("class".into(), "item".into());
            }
        }
        doc.append_element(body, "span");
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                r#"[document.querySelectorAll("p.item").length, document.querySelectorAll("p, span").length, document.querySelector("p.item") !== null].join(",")"#
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("3,4,true"));
    }

    #[test]
    fn get_elements_by_class_name_returns_all_matches() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let html = doc.append_element(root, "html");
        let body = doc.append_element(html, "body");
        for _ in 0..2 {
            let d = doc.append_element(body, "div");
            if let dom::NodeData::Element(e) = &mut doc.get_mut(d).data {
                e.attrs.insert("class".into(), "foo bar".into());
            }
        }
        let lone = doc.append_element(body, "div");
        if let dom::NodeData::Element(e) = &mut doc.get_mut(lone).data {
            e.attrs.insert("class".into(), "foo".into());
        }
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                r#"[document.getElementsByClassName("foo").length, document.getElementsByClassName("foo bar").length].join(",")"#
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("3,2"));
    }

    #[test]
    fn descendant_and_compound_selectors_match() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let html = doc.append_element(root, "html");
        let body = doc.append_element(html, "body");
        let nav = doc.append_element(body, "nav");
        let a = doc.append_element(nav, "a");
        if let dom::NodeData::Element(e) = &mut doc.get_mut(a).data {
            e.attrs.insert("class".into(), "link".into());
        }
        // A second <a> outside nav should NOT match "nav a".
        doc.append_element(body, "a");
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                r#"[document.querySelectorAll("nav a").length, document.querySelectorAll("a.link").length].join(",")"#
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("1,1"));
    }

    #[test]
    fn document_fonts_load_returns_a_thenable() {
        let out = env_eval(
            "https://example.com/",
            "typeof document.fonts.load().then === 'function' && document.fonts.check() === true \
             && document.fonts.status === 'loaded' && typeof document.fonts.ready.then === 'function'",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("true"));
    }

    #[test]
    fn get_bounding_client_rect_shape() {
        let out = env_eval(
            "https://example.com/",
            "var r = document.body.getBoundingClientRect(); \
             [r.x, r.y, r.top, r.left, r.right, r.bottom, r.width, r.height].join(',')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("0,0,0,0,0,0,0,0"));
    }

    #[test]
    fn observers_and_performance_do_not_throw() {
        let out = env_eval(
            "https://example.com/",
            "var mo = new MutationObserver(function(){}); mo.observe(document.body, {}); mo.disconnect(); \
             var io = new IntersectionObserver(function(){}); io.observe(document.body); \
             var ro = new ResizeObserver(function(){}); ro.observe(document.body); \
             new PerformanceObserver(function(){}).observe({}); \
             typeof performance.now() === 'number' && performance.getEntriesByType('x').length === 0",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("true"));
    }

    #[test]
    fn url_and_search_params_work() {
        let out = env_eval(
            "https://example.com/",
            "var u = new URL('https://a.com/p?x=1&y=2#h'); \
             var sp = new URLSearchParams('a=1&b=2'); \
             [u.hostname, u.pathname, u.searchParams.get('x'), sp.get('b'), u.hash].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("a.com|/p|1|2|#h"));
    }

    #[test]
    fn dataset_reads_and_writes_data_attributes() {
        let (doc, body) = doc_with_body("");
        let (doc, out) = run_with_dom(
            doc,
            vec![r#"var b = document.body; b.dataset.fooBar = "1"; b.setAttribute("data-baz", "2"); b.dataset.fooBar + "|" + b.dataset.baz"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("1|2"));
        assert_eq!(attr_of(&doc, body, "data-foo-bar").as_deref(), Some("1"));
    }

    #[test]
    fn create_text_node_and_fragment_present() {
        let out = env_eval(
            "https://example.com/",
            "var t = document.createTextNode('hi'); var f = document.createDocumentFragment(); \
             var c = document.createComment('x'); \
             [t.nodeType, t.data, f.nodeType, c.nodeType].join(',')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("3,hi,11,8"));
    }

    #[test]
    fn created_text_comment_have_working_parent_chain() {
        // Text + comment nodes are real arena nodes: once appended they expose a live parentNode and
        // can be used as insertBefore anchors (the fragment-anchor pattern Vue relies on).
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var anchor = document.createComment('');
                document.body.appendChild(anchor);
                var t = document.createTextNode('x');
                anchor.parentNode.insertBefore(t, anchor);
                t.nodeValue = 'y';
                [anchor.parentNode.nodeName, t.parentNode.__node === anchor.parentNode.__node, t.data].join('|')
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("BODY|true|y"));
    }

    #[test]
    fn insert_adjacent_html_inserts_parsed_nodes() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var el = document.createElement('div');
                document.body.appendChild(el);
                el.insertAdjacentHTML('beforeend', '<b>x</b>');
                var a = el.children[0].tagName;
                el.insertAdjacentHTML('afterbegin', '<i>y</i>');
                var b = el.children[0].tagName;
                var c = el.children[1].tagName;
                [a, b, c].join('|')
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        // beforeend appended B; afterbegin then put I first, B second.
        assert_eq!(out[0].value.as_deref(), Some("B|I|B"));
    }

    #[test]
    fn navigation_accessors_and_enrichment_propagate() {
        // A child reached via navigation must itself be enriched (style write-through works).
        let mut doc = dom::Document::new();
        let root = doc.root();
        let html = doc.append_element(root, "html");
        let body = doc.append_element(html, "body");
        let child = doc.append_element(body, "div");
        let (doc, out) = run_with_dom(
            doc,
            vec![r#"var c = document.body.firstElementChild; c.style.display = "block"; c.tagName"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("DIV"));
        let style = attr_of(&doc, child, "style").unwrap_or_default();
        assert!(style.contains("display: block"), "child style attr was {style:?}");
    }

    #[test]
    fn ready_state_advances_to_complete_after_drain() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"document.addEventListener("DOMContentLoaded", function () { console.log("dcl:" + document.readyState); });
                    window.addEventListener("load", function () { console.log("load:" + document.readyState); });"#
                .to_string()],
            "https://example.com/",
        );
        let all: Vec<String> = out.iter().flat_map(|o| o.console.clone()).collect();
        assert!(all.iter().any(|l| l == "dcl:interactive"), "got {all:?}");
        assert!(all.iter().any(|l| l == "load:complete"), "got {all:?}");
    }

    // --- ES modules (`run_modules`) -----------------------------------------------------

    /// Collect every console line across all of `run_modules`'s outputs.
    fn all_console(out: &[EvalOutput]) -> Vec<String> {
        out.iter().flat_map(|o| o.console.clone()).collect()
    }

    /// A fetcher that never serves anything (the static `modules` map is the only source).
    fn no_fetch() -> Box<dyn Fn(&str) -> Option<String> + Send> {
        Box::new(|_u: &str| None)
    }

    #[test]
    fn two_module_graph_resolves_named_import() {
        let entry = "https://x/app.js".to_string();
        let util = "https://x/util.js".to_string();
        let mut modules = std::collections::HashMap::new();
        // Specifiers are pre-canonicalized (the engine rewrites them to absolute URLs).
        modules.insert(
            entry.clone(),
            r#"import { v } from "https://x/util.js"; console.log("got", v);"#.to_string(),
        );
        modules.insert(util, "export const v = 42;".to_string());

        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules, no_fetch());
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(console.iter().any(|l| l == "got 42"), "console was {console:?}");
    }

    #[test]
    fn export_from_reexport_chain_resolves() {
        let entry = "https://x/app.js".to_string();
        let mid = "https://x/mid.js".to_string();
        let leaf = "https://x/leaf.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"import { hello } from "https://x/mid.js"; console.log(hello());"#.to_string(),
        );
        modules.insert(mid, r#"export * from "https://x/leaf.js";"#.to_string());
        modules.insert(leaf, r#"export function hello() { return "chained"; }"#.to_string());

        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules, no_fetch());
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(console.iter().any(|l| l == "chained"), "console was {console:?}");
    }

    #[test]
    fn import_meta_url_is_module_canonical_url() {
        let entry = "https://x/sub/mod.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"console.log(import.meta.url);
               console.log(import.meta.resolve("./other.css"));"#
                .to_string(),
        );

        let (doc, _) = doc_with_body("");
        let (_doc, out) =
            run_modules(doc, "https://x/", vec![entry.clone()], modules, no_fetch());
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(
            console.iter().any(|l| l == &entry),
            "import.meta.url should be {entry}, console was {console:?}"
        );
        // resolve() builds the URL relative to the module's own URL (the exact dot-normalization
        // depends on the environment's `URL` shim; what matters is the base is the module URL).
        assert!(
            console.iter().any(|l| l.starts_with("https://x/sub/") && l.ends_with("other.css")),
            "import.meta.resolve should resolve relative to the module URL, console was {console:?}"
        );
    }

    #[test]
    fn missing_module_surfaces_error_without_panic() {
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        // Imports a module that isn't present in the map.
        modules.insert(
            entry.clone(),
            r#"import { gone } from "https://x/missing.js"; console.log(gone);"#.to_string(),
        );

        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules, no_fetch());
        // Must not panic; the entry's evaluation surfaces an error.
        assert!(out.iter().any(|o| o.error.is_some()), "expected an error, got {out:?}");
    }

    #[test]
    fn side_effect_import_runs_imported_module() {
        let entry = "https://x/app.js".to_string();
        let dep = "https://x/dep.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(entry.clone(), r#"import "https://x/dep.js";"#.to_string());
        modules.insert(dep, r#"console.log("side effect ran");"#.to_string());

        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules, no_fetch());
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(
            console.iter().any(|l| l == "side effect ran"),
            "console was {console:?}"
        );
    }

    #[test]
    fn fetch_resolves_and_json_parses_via_host_fetcher() {
        // A module fetches a relative URL; the host fetcher serves canned JSON. The Response's
        // .json() must parse it and the value must reach the console (proving fetch + Promise drain).
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"fetch("data.json").then(r => r.json()).then(d => console.log("got:" + d.score));"#
                .to_string(),
        );

        let fetcher: Box<dyn Fn(&str) -> Option<String> + Send> = Box::new(|u: &str| {
            // The URL is resolved against the page URL before reaching the host fetcher.
            assert_eq!(u, "https://x/data.json", "fetch should resolve relative URLs");
            Some(r#"{"score": 99}"#.to_string())
        });

        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules, fetcher);
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(console.iter().any(|l| l == "got:99"), "console was {console:?}");
    }

    #[test]
    fn fetch_rejects_with_typeerror_when_host_fetch_fails() {
        // When the host fetcher returns None, fetch() rejects with a TypeError("Failed to fetch").
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"fetch("nope.json").catch(e => console.log("caught:" + e.name + ":" + e.message));"#
                .to_string(),
        );

        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules, no_fetch());
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(
            console.iter().any(|l| l == "caught:TypeError:Failed to fetch"),
            "console was {console:?}"
        );
    }

    #[test]
    fn svg_baseval_stub_does_not_throw() {
        // Reading SVG geometry props (width/height/viewBox .baseVal) must not throw.
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"var svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
               console.log("dims:" + svg.width.baseVal.value + "," + svg.height.baseVal.value
                 + "," + svg.viewBox.baseVal.width);"#
                .to_string(),
        );

        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules, no_fetch());
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(console.iter().any(|l| l == "dims:0,0,0"), "console was {console:?}");
    }

    #[test]
    fn modules_see_document_global() {
        // A module can touch the shared DOM-wired `document`/`window`, like page scripts.
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"document.title = "from-module"; console.log("title:" + document.title);"#.to_string(),
        );

        let (doc, _) = doc_with_body("orig");
        let (doc, out) = run_modules(doc, "https://x/", vec![entry], modules, no_fetch());
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(console.iter().any(|l| l == "title:from-module"), "console was {console:?}");
        // The mutation is visible in the returned document.
        let title = find_by_tag(&doc, doc.root(), "title").map(|n| text_content(&doc, n));
        assert_eq!(title.as_deref(), Some("from-module"));
    }

    #[test]
    fn dynamic_import_of_on_demand_fetched_module_resolves() {
        // Module A is in the pre-fetched map and dynamically imports B at runtime. B is provided
        // ONLY by the fetcher (not in `modules`), simulating browserscore's per-feature modules
        // computed at runtime. The dynamic import must resolve and B's export be observed.
        let entry = "https://x/a.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"const m = await import("https://x/b.js"); console.log("dyn:" + m.answer);"#
                .to_string(),
        );

        let fetcher: Box<dyn Fn(&str) -> Option<String> + Send> = Box::new(|u: &str| {
            if u == "https://x/b.js" {
                Some("export const answer = 99;".to_string())
            } else {
                None
            }
        });

        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules, fetcher);
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(console.iter().any(|l| l == "dyn:99"), "console was {console:?}");
    }

    // --- DOM interface globals + element identity / expandos -----------------------------

    #[test]
    fn dom_interface_globals_are_constructors_with_prototypes() {
        let out = env_eval(
            "https://example.com/",
            "[typeof Node, typeof Element, typeof HTMLElement, typeof HTMLUnknownElement, \
              typeof SVGElement, typeof Text, typeof Comment, typeof DocumentFragment, \
              typeof HTMLDivElement, typeof CharacterData, typeof Event, typeof CustomEvent].join(',') \
             + '|' + (HTMLElement.prototype && Element.prototype && Node.prototype ? 'protos' : 'no') \
             + '|' + (Object.getPrototypeOf(HTMLDivElement.prototype) === HTMLElement.prototype) \
             + '|' + (Object.getPrototypeOf(HTMLElement.prototype) === Element.prototype) \
             + '|' + (Object.getPrototypeOf(Element.prototype) === Node.prototype)",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(
            out.value.as_deref(),
            Some("function,function,function,function,function,function,function,function,function,function,function,function|protos|true|true|true")
        );
    }

    #[test]
    fn style_element_exposes_sheet_with_css_rules() {
        // Feature-detection libs (e.g. browserscore) read `styleEl.sheet.cssRules[0].cssText`.
        let out = env_eval(
            "https://example.com/",
            "var s = document.createElement('style'); \
             document.documentElement.appendChild(s); \
             s.textContent = 'a { color: red }'; \
             [typeof s.sheet, s.sheet.cssRules.length, s.sheet.cssRules[0].cssText].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("object|1|a { color: red }"));
    }

    #[test]
    fn created_element_is_instanceof_html_element_and_node() {
        let out = env_eval(
            "https://example.com/",
            "var d = document.createElement('div'); \
             [d instanceof HTMLDivElement, d instanceof HTMLElement, d instanceof Element, \
              d instanceof Node, d instanceof SVGElement].join(',')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("true,true,true,true,false"));
    }

    #[test]
    fn expando_set_on_created_element_persists_and_identity_is_stable() {
        // Vue stashes internal state directly on DOM nodes (el.__vnode, el._vei). A node looked
        // up twice must be the SAME JS object so those expandos survive.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let html = doc.append_element(root, "html");
        let body = doc.append_element(html, "body");
        let p = doc.append_element(body, "p");
        if let dom::NodeData::Element(e) = &mut doc.get_mut(p).data {
            e.attrs.insert("id".into(), "t".into());
        }
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var a = document.getElementById("t");
                a.__vnode = { stashed: 42 };
                a._vei = "x";
                var b = document.getElementById("t");
                [a === b, b.__vnode && b.__vnode.stashed, b._vei,
                 document.body.firstElementChild === a].join("|")
            "#.to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        // Same identity, expandos visible on the second lookup, and navigation returns the same obj.
        assert_eq!(out[0].value.as_deref(), Some("true|42|x|true"));
    }

    #[test]
    fn created_element_accepts_arbitrary_expando_properties() {
        let out = env_eval(
            "https://example.com/",
            "var el = document.createElement('div'); \
             el.$once = function () { return 7; }; el.__custom = { k: 1 }; \
             document.body.appendChild(el); \
             var same = document.body.lastChild; \
             [same.$once ? same.$once() : 'no', same.__custom ? same.__custom.k : 'no', same === el].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("7|1|true"));
    }

    #[test]
    fn create_element_ns_returns_enriched_appendable_element() {
        // Vue's runtime-dom createElement uses document.createElementNS for SVG/MathML. The result
        // must be a real, enriched element (appendChild/setAttribute present) and record namespaceURI.
        let out = env_eval(
            "https://example.com/",
            "var ns = 'http://www.w3.org/2000/svg'; \
             var el = document.createElementNS(ns, 'svg:path'); \
             el.setAttribute('d', 'M0 0'); \
             document.body.appendChild(el); \
             [el.tagName.toLowerCase(), el.namespaceURI === ns, \
              typeof el.appendChild, document.body.lastChild === el].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("path|true|function|true"));
    }

    // --- persistent Session ------------------------------------------------------------------

    #[test]
    fn session_click_handler_mutates_dom() {
        let doc = html::parse("<button id=btn></button><span id=out>idle</span>");
        let (session, snapshot, outputs) = Session::new(
            doc,
            vec![r#"
                var b = document.getElementById('btn');
                b.addEventListener('click', function () {
                    document.getElementById('out').textContent = 'clicked';
                });
            "#
            .to_string()],
            Vec::new(),
            HashMap::new(),
            "https://example.com/",
            no_fetch(),
        );
        assert_eq!(outputs[0].error, None, "{:?}", outputs[0]);

        // Find the button's node id in the returned snapshot.
        let btn = find_by_id(&snapshot, snapshot.root(), "btn").expect("btn node");

        let (after, _console) = session.dispatch_event(btn.0, "click", 0.0, 0.0);
        let out = find_by_id(&after, after.root(), "out").expect("out node");
        assert_eq!(text_content(&after, out), "clicked");
    }

    #[test]
    fn session_timer_runs_on_tick() {
        let doc = html::parse("<body></body>");
        // An interval fires during load, then again over real time as ticks pump the loop.
        let (session, snapshot, outputs) = Session::new(
            doc,
            vec![r#"globalThis.__c = 0; setInterval(function () { globalThis.__c++; document.body.setAttribute('data-c', String(globalThis.__c)); }, 30);"#
                .to_string()],
            Vec::new(),
            HashMap::new(),
            "https://example.com/",
            no_fetch(),
        );
        assert_eq!(outputs[0].error, None, "{:?}", outputs[0]);
        // Ran at least once during load.
        let body0 = find_by_tag(&snapshot, snapshot.root(), "body").expect("body node");
        let initial: i32 = attr_of(&snapshot, body0, "data-c").unwrap_or_default().parse().unwrap_or(0);
        assert!(initial >= 1, "interval should run during load, got {initial}");

        // After real time elapses, a tick fires it again (real-clock cadence) → count increases.
        std::thread::sleep(std::time::Duration::from_millis(80));
        let (after, _console) = session.tick().expect("interval should fire again on tick");
        let body = find_by_tag(&after, after.root(), "body").expect("body node");
        let c: i32 = attr_of(&after, body, "data-c").unwrap_or_default().parse().unwrap_or(0);
        assert!(c > initial, "interval should have fired again on tick: {initial} -> {c}");
    }

    #[test]
    fn session_event_bubbles_to_ancestor() {
        let doc = html::parse(
            "<div id=parent><button id=child></button></div><span id=out>idle</span>",
        );
        let (session, snapshot, outputs) = Session::new(
            doc,
            vec![r#"
                var p = document.getElementById('parent');
                p.addEventListener('click', function () {
                    document.getElementById('out').textContent = 'bubbled';
                });
            "#
            .to_string()],
            Vec::new(),
            HashMap::new(),
            "https://example.com/",
            no_fetch(),
        );
        assert_eq!(outputs[0].error, None, "{:?}", outputs[0]);

        let child = find_by_id(&snapshot, snapshot.root(), "child").expect("child node");
        let (after, _console) = session.dispatch_event(child.0, "click", 0.0, 0.0);
        let out = find_by_id(&after, after.root(), "out").expect("out node");
        assert_eq!(text_content(&after, out), "bubbled");
    }

    #[test]
    fn session_key_input_appends_and_fires_input_handler() {
        let doc = html::parse("<html><body><input id=f></body></html>");
        let (session, snapshot, outputs) = Session::new(
            doc,
            vec![r#"
                var i = document.getElementById('f');
                i.addEventListener('input', function () {
                    document.body.setAttribute('data-v', i.value);
                });
            "#
            .to_string()],
            Vec::new(),
            HashMap::new(),
            "https://example.com/",
            no_fetch(),
        );
        assert_eq!(outputs[0].error, None, "{:?}", outputs[0]);

        let f = find_by_id(&snapshot, snapshot.root(), "f").expect("input node");
        let (_after, _c) = session.dispatch_key(f.0, "a", "KeyA");
        let (after, _c) = session.dispatch_key(f.0, "b", "KeyB");

        let input = find_by_id(&after, after.root(), "f").expect("input node");
        assert_eq!(attr_of(&after, input, "value").as_deref(), Some("ab"));
        let body = find_by_tag(&after, after.root(), "body").expect("body node");
        assert_eq!(attr_of(&after, body, "data-v").as_deref(), Some("ab"));
    }

    #[test]
    fn session_key_backspace_drops_last_char() {
        let doc = html::parse("<input id=f value=hi>");
        let (session, snapshot, outputs) = Session::new(
            doc,
            Vec::new(),
            Vec::new(),
            HashMap::new(),
            "https://example.com/",
            no_fetch(),
        );
        assert!(outputs.is_empty() || outputs.iter().all(|o| o.error.is_none()));

        let f = find_by_id(&snapshot, snapshot.root(), "f").expect("input node");
        let (after, _c) = session.dispatch_key(f.0, "Backspace", "Backspace");
        let input = find_by_id(&after, after.root(), "f").expect("input node");
        assert_eq!(attr_of(&after, input, "value").as_deref(), Some("h"));
    }

    #[test]
    fn session_toggle_checkbox_flips_checked_and_fires_change() {
        let doc = html::parse(
            "<html><body><input id=c type=checkbox></body></html>",
        );
        let (session, snapshot, outputs) = Session::new(
            doc,
            vec![r#"
                var c = document.getElementById('c');
                c.addEventListener('change', function () {
                    document.body.setAttribute('data-changed', c.checked ? 'on' : 'off');
                });
            "#
            .to_string()],
            Vec::new(),
            HashMap::new(),
            "https://example.com/",
            no_fetch(),
        );
        assert_eq!(outputs[0].error, None, "{:?}", outputs[0]);

        let c = find_by_id(&snapshot, snapshot.root(), "c").expect("checkbox node");
        // Initially unchecked.
        assert!(attr_of(&snapshot, c, "checked").is_none());

        let (after, _console) = session.toggle_checkbox(c.0);
        let cb = find_by_id(&after, after.root(), "c").expect("checkbox node");
        assert!(attr_of(&after, cb, "checked").is_some(), "checkbox should be checked");
        let body = find_by_tag(&after, after.root(), "body").expect("body node");
        assert_eq!(attr_of(&after, body, "data-changed").as_deref(), Some("on"));

        // Toggling again unchecks it (and the change handler sees the new state).
        let (after2, _c2) = session.toggle_checkbox(c.0);
        let cb2 = find_by_id(&after2, after2.root(), "c").expect("checkbox node");
        assert!(attr_of(&after2, cb2, "checked").is_none(), "checkbox should be unchecked");
        let body2 = find_by_tag(&after2, after2.root(), "body").expect("body node");
        assert_eq!(attr_of(&after2, body2, "data-changed").as_deref(), Some("off"));
    }

    #[test]
    fn session_toggle_radio_unchecks_same_name_sibling() {
        let doc = html::parse(
            "<form>\
               <input id=a type=radio name=g checked>\
               <input id=b type=radio name=g>\
             </form>",
        );
        let (session, snapshot, outputs) = Session::new(
            doc,
            Vec::new(),
            Vec::new(),
            HashMap::new(),
            "https://example.com/",
            no_fetch(),
        );
        assert!(outputs.iter().all(|o| o.error.is_none()));

        let a = find_by_id(&snapshot, snapshot.root(), "a").expect("radio a");
        let b = find_by_id(&snapshot, snapshot.root(), "b").expect("radio b");
        assert!(attr_of(&snapshot, a, "checked").is_some());
        assert!(attr_of(&snapshot, b, "checked").is_none());

        // Check b: a (same name) must become unchecked.
        let (after, _console) = session.toggle_checkbox(b.0);
        let aa = find_by_id(&after, after.root(), "a").expect("radio a");
        let bb = find_by_id(&after, after.root(), "b").expect("radio b");
        assert!(attr_of(&after, bb, "checked").is_some(), "b should be checked");
        assert!(attr_of(&after, aa, "checked").is_none(), "a should be unchecked");
    }

    #[test]
    fn session_hover_reaches_mouseover_listener() {
        let doc = html::parse("<html><body><div id=menu>menu</div></body></html>");
        let (session, snapshot, outputs) = Session::new(
            doc,
            vec![r#"
                var m = document.getElementById('menu');
                m.addEventListener('mouseover', function () {
                    document.body.setAttribute('data-hover', 'yes');
                });
            "#
            .to_string()],
            Vec::new(),
            HashMap::new(),
            "https://example.com/",
            no_fetch(),
        );
        assert_eq!(outputs[0].error, None, "{:?}", outputs[0]);

        let menu = find_by_id(&snapshot, snapshot.root(), "menu").expect("menu node");
        let (after, _console) = session.dispatch_event(menu.0, "mouseover", 5.0, 5.0);
        let body = find_by_tag(&after, after.root(), "body").expect("body node");
        assert_eq!(attr_of(&after, body, "data-hover").as_deref(), Some("yes"));
    }

    #[test]
    fn session_nonbubbling_focus_does_not_reach_ancestor() {
        let doc = html::parse(
            "<html><body><div id=wrap><input id=f></div></body></html>",
        );
        let (session, snapshot, outputs) = Session::new(
            doc,
            vec![r#"
                var f = document.getElementById('f');
                f.addEventListener('focus', function () {
                    document.body.setAttribute('data-target', 'focused');
                });
                document.getElementById('wrap').addEventListener('focus', function () {
                    document.body.setAttribute('data-ancestor', 'reached');
                });
            "#
            .to_string()],
            Vec::new(),
            HashMap::new(),
            "https://example.com/",
            no_fetch(),
        );
        assert_eq!(outputs[0].error, None, "{:?}", outputs[0]);

        let f = find_by_id(&snapshot, snapshot.root(), "f").expect("input node");
        let (after, _console) = session.fire_event_nonbubbling(f.0, "focus");
        let body = find_by_tag(&after, after.root(), "body").expect("body node");
        // The target's focus handler ran...
        assert_eq!(attr_of(&after, body, "data-target").as_deref(), Some("focused"));
        // ...but the ancestor's did NOT (focus does not bubble).
        assert_eq!(attr_of(&after, body, "data-ancestor").as_deref(), None);
    }
}
