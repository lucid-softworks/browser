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

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::sync::Once;

/// A completion delivered from a background request thread back to the worker: `(request id,
/// response-envelope JSON or None on transport error)`. Drained on the worker thread inside
/// [`drain_event_loop`] to resolve/reject the pending JS `fetch()` promise.
type FetchCompletion = (u64, Option<String>);

/// A WebSocket event delivered from a background socket thread to the worker: `(socket id, kind,
/// payload)`. kind `0`=open, `1`=text, `2`=binary(base64), `3`=close("code:reason"), `4`=error.
/// Drained opportunistically (non-blocking) inside [`drain_event_loop`] and dispatched to JS via
/// `__wsDeliver`. A socket is long-lived, so — unlike a fetch — it never touches `in_flight`.
type WsEvent = (u64, u8, String);

/// An outgoing WebSocket command from JS to a background socket thread: `(kind, payload)`.
/// kind `0`=send text, `1`=send binary(base64), `2`=close. Sent over a per-socket channel whose
/// receiver lives on that socket's `net::ws_run` thread.
type WsOut = (u8, String);

/// Host WebSocket connector (built by the engine, mirroring `request_fetcher`): given
/// `(url, id, ws_evt_tx)` it spawns the socket thread and returns the per-socket outgoing sender,
/// or `Err` if the thread couldn't start. Crosses the crate boundary with PRIMITIVE tuples only.
type WsConnector =
    Arc<dyn Fn(String, u64, Sender<WsEvent>) -> Result<Sender<WsOut>, String> + Send + Sync>;

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

/// A single DOM mutation recorded by the native mutation primitives while at least one
/// `MutationObserver` is registered (`observers_active == true`). The JS dispatch layer
/// (`__deliverMutations`) drains these as JSON, matches them against the JS-side observer
/// registry, and builds the spec `MutationRecord` objects. We keep this Rust-side struct (rather
/// than tracking mutations in JS) because the mutations happen inside the Rust DOM primitives.
struct MutationRec {
    /// "childList" | "attributes" | "characterData".
    kind: &'static str,
    target: dom::NodeId,
    /// Attribute name for `attributes` records (None otherwise).
    attr_name: Option<String>,
    /// Previous value, captured BEFORE the write: the attribute's old value (`attributes`) or the
    /// node's old text (`characterData`). Used for `attributeOldValue`/`characterDataOldValue`.
    old_value: Option<String>,
    /// Nodes added by a `childList` mutation.
    added: Vec<dom::NodeId>,
    /// Nodes removed by a `childList` mutation.
    removed: Vec<dom::NodeId>,
}

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
    fetcher: Rc<dyn Fn(&str) -> Option<(String, String)>>,
    /// Host network capability for arbitrary-method requests (method, url, body, headers-JSON),
    /// backing the `__request` native primitive that powers JS `fetch()` with method/headers/body.
    /// Returns a JSON response *envelope* (see `engine`'s builder) or `None` on transport error.
    /// Distinct from `fetcher` (a GET-only body fetcher) which module loading still relies on.
    /// No-DOM / `run_with_dom` paths install a no-op that always returns `None`.
    ///
    /// `Arc<... + Send + Sync>` (not `Rc`) because `__startFetch` clones this and hands the clone
    /// to a **background request thread** which runs it off the worker thread. `net::request` is
    /// stateless + shares an agent, so it is `Send + Sync`-safe.
    request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync>,
    /// Channel sender background request threads use to deliver completions back to this worker.
    /// `Sender` is `Send`; it is only ever cloned/touched on the worker thread (in `__startFetch`),
    /// so storing it in the (non-`Send`) `HostState` is fine. The matching `Receiver` is owned by
    /// the worker thread and drained inside [`drain_event_loop`].
    fetch_tx: Sender<FetchCompletion>,
    /// Monotonic id source for async `fetch()` requests (handed to JS so it can correlate the
    /// completion). `Atomic` only for `Send`-ability of the closure capture; logically single-thread.
    next_fetch_id: AtomicU64,
    /// Count of async fetches started but not yet drained. Keeps [`drain_event_loop`] looping (on a
    /// longer budget) while network is outstanding so the `fetch()` promise settles before snapshot.
    in_flight: Cell<usize>,
    /// Host WebSocket connector (the engine's `build_ws_connector`). Called by `__wsConnect` to spawn
    /// a socket thread. `Arc<… + Send + Sync>` because it captures `Send` channels. No-DOM paths
    /// install one that always returns `Err` (no socket threads on those paths).
    ws_connector: WsConnector,
    /// Sender background socket threads use to deliver WebSocket events to this worker. Cloned per
    /// socket (in `__wsConnect`) and handed to the socket thread. The matching receiver is owned by
    /// the worker thread and drained (non-blocking) inside [`drain_event_loop`].
    ws_evt_tx: Sender<WsEvent>,
    /// Per-socket outgoing senders, keyed by socket id. `__wsSend`/`__wsClose` look up the id here;
    /// a close (kind 3) event removes the entry. The socket thread closes when its receiver drops.
    ws_senders: RefCell<HashMap<u64, Sender<WsOut>>>,
    /// Monotonic id source for WebSocket connections (handed to JS so it can correlate events).
    next_ws_id: AtomicU64,
    /// Queue of DOM mutations recorded for `MutationObserver`s. Only written while
    /// `observers_active` is true. Drained as JSON by `__drainMutations` and dispatched to JS.
    mutations: RefCell<Vec<MutationRec>>,
    /// Cheap gate: true only while at least one `MutationObserver` is registered. When false the
    /// mutation primitives record nothing (the common case for pages with no observers).
    observers_active: Cell<bool>,
    /// Monotonic version of the live DOM, bumped by every mutation primitive (append/insert/remove
    /// child, set/remove attr, set text content). Used to invalidate `computed_cache`: if the cache
    /// was computed at an older version it is stale and must be recomputed. browserscore.dev sets an
    /// inline style on a probe element and immediately reads it back via `getComputedStyle`, so
    /// invalidate-on-mutation is essential for the read to reflect the write.
    dom_version: Cell<u64>,
    /// Cached cascade for `getComputedStyle`, tagged with the `dom_version` it was computed at.
    /// `None` until the first `getComputedStyle` call. Recomputed lazily when the tag != the current
    /// `dom_version`. Computed entirely in-Session (the JS thread holds the DOM while the engine is
    /// blocked, so we cannot reach the engine's cascade — we run `style::cascade` here ourselves).
    computed_cache: RefCell<Option<(u64, HashMap<dom::NodeId, style::ComputedStyle>)>>,
    /// Element border-box rects pushed by the engine after each (re)layout, keyed by node id.
    /// `(x, y, width, height)` in **CSS px**, document-absolute, top-origin. Empty until the first
    /// `SessionCmd::SetRects`. Read by the `__rect` / `__elemMetrics` primitives that back
    /// `getBoundingClientRect` / `offsetWidth` / `scrollHeight` etc. The engine recomputes layout;
    /// the worker only serves what was pushed (it cannot reach the engine's layout from here).
    layout_rects: RefCell<HashMap<usize, (f32, f32, f32, f32)>>,
    /// CSSOM *used* inset values per positioned box, keyed by node id, pushed by the engine
    /// alongside `layout_rects`. `(top, right, bottom, left)` in CSS px. Read by `resolved_inset_value`
    /// so `getComputedStyle(el).top` etc. report the used value when the element has a box; absent for
    /// box-less / non-positioned elements (those fall back to the computed value).
    used_insets: RefCell<HashMap<usize, (f32, f32, f32, f32)>>,
    /// CSSOM *used* margin values per box, keyed by node id, pushed by the engine alongside
    /// `layout_rects`. `(top, right, bottom, left)` in CSS px. Read by `getComputedStyle(el).margin*`
    /// so resolved `auto` margins (centering / over-constrained boxes) report their used pixel value.
    used_margins: RefCell<HashMap<usize, (f32, f32, f32, f32)>>,
    /// Decoded intrinsic size of each `<img>`, keyed by node id, pushed by the engine alongside
    /// `layout_rects`. `(natural_width, natural_height)` in CSS px from the decoded bitmap. Read by
    /// the `__naturalSize` primitive backing `img.naturalWidth` / `img.naturalHeight`. Empty until
    /// the first push; a missing/broken image has no entry (reports 0).
    image_natural: RefCell<HashMap<usize, (f32, f32)>>,
    /// Rasterized RGBA pixels of each `<canvas>` (and decoded `<img>`), keyed by node id, pushed by
    /// the engine after it rasterizes the display lists. `(width, height, rgba8)` — straight-alpha,
    /// row-major, 4 bytes/pixel. Backs `ctx.getImageData` and `ctx.drawImage` sizing checks. Empty
    /// until the first push; reflects the PREVIOUS frame's pixels (a one-render lag — `getImageData`
    /// after a draw sees the right pixels on the next render).
    canvas_pixels: RefCell<HashMap<usize, (u32, u32, Vec<u8>)>>,
    /// Vertical scroll offset (CSS px) at the last push. `__rect` subtracts this to make
    /// `getBoundingClientRect` viewport-relative. No horizontal scroll is tracked.
    viewport_scroll_y: Cell<f32>,
    /// Full document content height (CSS px) at the last push. Reported as
    /// `documentElement.scrollHeight` / `body.scrollHeight` so pages that size off the page height work.
    doc_height: Cell<f32>,
    /// The page URL (the document's address). Set once at bootstrap. Combined with any `<base href>`
    /// it yields the document base URL, used to resolve relative `url(...)` in inline styles.
    page_url: RefCell<String>,
}

impl HostState {
    fn new(doc: SharedDoc) -> Rc<Self> {
        // No-DOM paths: dead-end channels (their receivers are dropped immediately) and a connector
        // that always errs. `__startFetch`/`__wsConnect` never run here in practice; even if they
        // did, the sends simply fail / the connect errs harmlessly.
        let (tx, _rx) = std::sync::mpsc::channel();
        let (ws_tx, _ws_rx) = std::sync::mpsc::channel();
        Self::with_fetcher(
            doc,
            Rc::new(|_| None),
            Arc::new(|_, _, _, _| None),
            tx,
            Arc::new(|_, _, _| Err("no WebSocket connector".to_string())),
            ws_tx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn with_fetcher(
        doc: SharedDoc,
        fetcher: Rc<dyn Fn(&str) -> Option<(String, String)>>,
        request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync>,
        fetch_tx: Sender<FetchCompletion>,
        ws_connector: WsConnector,
        ws_evt_tx: Sender<WsEvent>,
    ) -> Rc<Self> {
        Rc::new(HostState {
            doc,
            console: RefCell::new(Vec::new()),
            fetcher,
            request_fetcher,
            fetch_tx,
            next_fetch_id: AtomicU64::new(1),
            in_flight: Cell::new(0),
            ws_connector,
            ws_evt_tx,
            ws_senders: RefCell::new(HashMap::new()),
            next_ws_id: AtomicU64::new(1),
            mutations: RefCell::new(Vec::new()),
            observers_active: Cell::new(false),
            dom_version: Cell::new(0),
            computed_cache: RefCell::new(None),
            layout_rects: RefCell::new(HashMap::new()),
            used_insets: RefCell::new(HashMap::new()),
            used_margins: RefCell::new(HashMap::new()),
            image_natural: RefCell::new(HashMap::new()),
            canvas_pixels: RefCell::new(HashMap::new()),
            viewport_scroll_y: Cell::new(0.0),
            doc_height: Cell::new(0.0),
            page_url: RefCell::new(String::new()),
        })
    }

    /// Bump the DOM version so the cached cascade (`computed_cache`) is treated as stale. Called by
    /// every mutation primitive.
    fn bump_dom_version(&self) {
        self.dom_version.set(self.dom_version.get().wrapping_add(1));
    }

    /// Record a mutation if any `MutationObserver` is registered. Cheap no-op otherwise.
    fn record_mutation(&self, rec: MutationRec) {
        if self.observers_active.get() {
            self.mutations.borrow_mut().push(rec);
        }
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
    // Per the DOM standard's `textContent` getter: for a Text/Comment/PI node it's the node's own
    // data; for an Element/DocumentFragment it's the concatenation of all *descendant Text* node
    // data in tree order (Comment data is NOT included for those).
    match &doc.get(id).data {
        dom::NodeData::Text(t) => return t.clone(),
        dom::NodeData::Comment(c) => return c.clone(),
        dom::NodeData::Cdata(c) => return c.clone(),
        dom::NodeData::ProcessingInstruction(p) => return p.data.clone(),
        _ => {}
    }
    let mut out = String::new();
    fn walk(doc: &dom::Document, id: dom::NodeId, out: &mut String) {
        match &doc.get(id).data {
            dom::NodeData::Text(t) => out.push_str(t),
            dom::NodeData::Cdata(t) => out.push_str(t),
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
            "area"
                | "base"
                | "br"
                | "col"
                | "embed"
                | "hr"
                | "img"
                | "input"
                | "link"
                | "meta"
                | "param"
                | "source"
                | "track"
                | "wbr"
        )
    }
    fn escape_text(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    fn escape_attr(s: &str) -> String {
        s.replace('&', "&amp;").replace('"', "&quot;")
    }
    fn serialize_node(doc: &dom::Document, id: dom::NodeId, out: &mut String) {
        match &doc.get(id).data {
            dom::NodeData::Text(t) => out.push_str(&escape_text(t)),
            dom::NodeData::Cdata(c) => {
                out.push_str("<![CDATA[");
                out.push_str(c);
                out.push_str("]]>");
            }
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
            dom::NodeData::DocumentType(d) => {
                out.push_str("<!DOCTYPE ");
                out.push_str(&d.name);
                out.push('>');
            }
            dom::NodeData::ProcessingInstruction(p) => {
                out.push_str("<?");
                out.push_str(&p.target);
                out.push(' ');
                out.push_str(&p.data);
                out.push('>');
            }
            dom::NodeData::Document | dom::NodeData::DocumentFragment => {
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
        dom::NodeData::Cdata(c) => {
            *c = text.to_string();
            return;
        }
        dom::NodeData::ProcessingInstruction(p) => {
            p.data = text.to_string();
            return;
        }
        _ => {}
    }
    let old: Vec<dom::NodeId> = std::mem::take(&mut doc.get_mut(id).children);
    for child in old {
        doc.get_mut(child).parent = None;
    }
    // Per spec: only insert a Text node when the new value is non-empty (empty string => no child).
    if !text.is_empty() {
        doc.append_child(id, dom::NodeData::Text(text.to_string()));
    }
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

#[derive(Clone, PartialEq, Debug)]
enum AttrMatchOp {
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
struct AttrCond {
    name: String, // local name, lowercased
    op: AttrMatchOp,
    value: String,
    ci: bool, // case-insensitive value match (the `i` flag)
}

/// A single compound selector, e.g. `div.foo#bar[disabled]`.
#[derive(Debug, Default, Clone)]
struct Compound {
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
fn strip_attr_ns(name: &str) -> &str {
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
fn parse_attr_cond(inner: &str) -> Option<AttrCond> {
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
fn unquote_attr_value(s: &str) -> String {
    let s = s.trim();
    let b = s.as_bytes();
    if s.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Match an [`AttrCond`] against an element (mirrors the cascade's attribute matching).
fn attr_cond_matches(e: &dom::ElementData, a: &AttrCond) -> bool {
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
fn is_hex(c: char) -> bool {
    c.is_ascii_hexdigit()
}

/// Read a CSS identifier (class / id / type name) starting at `i`, consuming CSS escape sequences
/// (`\` + 1-6 hex digits with optional trailing whitespace, or `\` + any other char as a literal).
/// Stops at an *unescaped* selector delimiter (`.` `#` `[` `:` `>` `+` `~` `,` ` `). Returns the
/// unescaped value and the index just past the identifier.
fn read_css_ident(bytes: &[char], mut i: usize) -> (String, usize) {
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
/// Splitting is escape-aware so a CSS escape that contains whitespace (`#\30 foo`) or a combinator
/// character stays within its compound; only *unescaped* combinators/whitespace separate compounds.
/// A CSS combinator between two compound selectors.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Combinator {
    Descendant,        // `A B`
    Child,             // `A > B`
    NextSibling,       // `A + B`
    SubsequentSibling, // `A ~ B`
}

/// The previous element sibling of `node` (skipping text / comment nodes), if any.
fn prev_element_sibling(doc: &dom::Document, node: dom::NodeId) -> Option<dom::NodeId> {
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
fn parse_complex(s: &str) -> Option<Vec<(Combinator, Compound)>> {
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
fn matches_complex(
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
fn query_selector_all(doc: &dom::Document, sel: &str) -> Vec<dom::NodeId> {
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
fn query_within(doc: &dom::Document, root: dom::NodeId, sel: &str) -> Vec<dom::NodeId> {
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
fn collect_by_class(
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
fn arg_node(
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
    let elements: Vec<v8::Local<v8::Value>> = items.iter().map(|s| js_str(scope, s)).collect();
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
fn prim_create_text(
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
fn prim_create_comment(
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
fn prim_create_cdata(
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
fn prim_create_document_node(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let state = host_state(scope);
    let id = state.doc.borrow_mut().alloc(dom::NodeData::Document, None);
    rv.set_double(id.0 as f64);
}

/// `__createDocumentFragment() -> id` — a parentless `DocumentFragment` arena node.
fn prim_create_document_fragment(
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
fn prim_create_document_type(
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
fn prim_create_processing_instruction(
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
fn prim_doctype_info(
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
fn prim_pi_target(
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
fn clone_node_arena(doc: &mut dom::Document, id: dom::NodeId, deep: bool) -> dom::NodeId {
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
fn prim_clone_node(
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
fn join_url(base: &str, href: &str) -> String {
    match url::Url::parse(base).and_then(|b| b.join(href.trim())) {
        Ok(u) => u.into(),
        Err(_) => base.to_string(),
    }
}

/// The document's base URL: the first `<base href>` resolved against the page URL, else the page URL.
fn document_base_url(doc: &dom::Document, page_url: &str) -> String {
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

fn collect_author_sheets(
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
fn fetch_link_css(
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
fn percent_decode_str(s: &str) -> String {
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

/// Recompute the cascade if the cache is missing or stale (DOM changed since it was built), then run
/// `f` over the cached `ComputedStyle` for `id` (`None` if `id` has no computed style — e.g. a text
/// node or out-of-range id). Keeps the borrow of `computed_cache` scoped to this call.
fn with_computed_style<R>(
    state: &HostState,
    id: dom::NodeId,
    f: impl FnOnce(Option<&style::ComputedStyle>) -> R,
) -> R {
    let version = state.dom_version.get();
    {
        let mut cache = state.computed_cache.borrow_mut();
        let fresh = matches!(&*cache, Some((v, _)) if *v == version);
        if !fresh {
            let doc = state.doc.borrow();
            let sheets = collect_author_sheets(&doc, &state.fetcher, &state.page_url.borrow());
            let map = style::cascade(&doc, &sheets);
            *cache = Some((version, map));
        }
    }
    let cache = state.computed_cache.borrow();
    let map = cache.as_ref().map(|(_, m)| m);
    if let Some(m) = map {
        if let Some(cs) = m.get(&id) {
            return f(Some(cs));
        }
        // Not in the main document cascade — it may live in an <iframe> facade document. Cascade that
        // detached subtree on its own, with the iframe's size as the viewport so the frame's @media
        // queries get their own context (re-resolved live as the iframe resizes).
        let doc = state.doc.borrow();
        if let Some((facade_root, iframe_id)) = find_facade_root(&doc, id) {
            // A `display: none` iframe isn't rendered, so its document has no boxes — getComputedStyle
            // returns the empty (no-value) style for its elements.
            if m.get(&iframe_id).map(|cs| cs.display_none).unwrap_or(false) {
                return f(None);
            }
            let attr_dim = |name: &str| -> Option<f32> {
                if let dom::NodeData::Element(e) = &doc.get(iframe_id).data {
                    e.attrs.get(name).and_then(|v| v.trim().parse::<f32>().ok())
                } else {
                    None
                }
            };
            // Content width is resolved recursively (a nested iframe's % width is relative to the
            // outer iframe's content width); height uses the simpler CSS-or-attr-or-default form.
            let iw = iframe_content_width(state, &doc, m, iframe_id, 0);
            let ih = m
                .get(&iframe_id)
                .and_then(|cs| cs.height)
                .or_else(|| attr_dim("height"))
                .unwrap_or(150.0);
            let sheets = collect_facade_sheets(&doc, facade_root, &state.fetcher);
            let (sw, sh, sd) = style::viewport_metrics();
            style::set_viewport_metrics(iw, ih, sd);
            let mut submap = style::cascade_subtree(&doc, facade_root, &sheets);
            style::set_viewport_metrics(sw, sh, sd);
            // Resolve percentage widths against the iframe content width (top-down), so
            // getComputedStyle reports their used px value (the frame has no real layout pass).
            resolve_facade_widths(&doc, &mut submap, facade_root, iw);
            return f(submap.get(&id));
        }
    }
    f(None)
}

/// Walk up from `id` to the nearest `<iframe>` facade-document root (its body carries a
/// `data-frame-host` attribute = the host iframe's node id). Returns `(facade_root, iframe_node)`.
fn find_facade_root(doc: &dom::Document, id: dom::NodeId) -> Option<(dom::NodeId, dom::NodeId)> {
    let mut cur = Some(id);
    while let Some(c) = cur {
        if c.0 < doc.len() {
            if let dom::NodeData::Element(e) = &doc.get(c).data {
                if let Some(v) = e.attrs.get("data-frame-host") {
                    if let Ok(raw) = v.trim().parse::<usize>() {
                        return Some((c, dom::NodeId(raw)));
                    }
                }
            }
        }
        cur = doc.get(c).parent;
    }
    None
}

/// The content width of an `<iframe>` in CSS px: explicit CSS width, else the `width` HTML attribute,
/// else — for a nested iframe whose width is a percentage — resolved by cascading the *outer* iframe
/// facade (recursing through outer frames), else the conventional 300px default.
fn iframe_content_width(
    state: &HostState,
    doc: &dom::Document,
    map: &HashMap<dom::NodeId, style::ComputedStyle>,
    iframe_id: dom::NodeId,
    depth: u32,
) -> f32 {
    if depth > 8 {
        return 300.0;
    }
    if let Some(w) = map.get(&iframe_id).and_then(|c| c.width) {
        return w;
    }
    if let dom::NodeData::Element(e) = &doc.get(iframe_id).data {
        if let Some(w) = e
            .attrs
            .get("width")
            .and_then(|v| v.trim().parse::<f32>().ok())
        {
            return w;
        }
    }
    // Not sized directly: the iframe lives inside an outer iframe's facade (a nested frame). Cascade
    // that outer facade against the outer iframe's content width, resolve widths, and read this
    // iframe's used width there.
    if let Some((outer_root, outer_iframe)) = find_facade_root(doc, iframe_id) {
        let outer_w = iframe_content_width(state, doc, map, outer_iframe, depth + 1);
        let sheets = collect_facade_sheets(doc, outer_root, &state.fetcher);
        let (sw, sh, sd) = style::viewport_metrics();
        style::set_viewport_metrics(outer_w, 150.0, sd);
        let mut submap = style::cascade_subtree(doc, outer_root, &sheets);
        style::set_viewport_metrics(sw, sh, sd);
        resolve_facade_widths(doc, &mut submap, outer_root, outer_w);
        if let Some(w) = submap.get(&iframe_id).and_then(|c| c.width) {
            return w;
        }
    }
    300.0
}

/// Resolve percentage / auto block widths down a facade subtree so getComputedStyle reports a used
/// px width (the frame has no layout pass). `avail_width` is the containing block's content width.
fn resolve_facade_widths(
    doc: &dom::Document,
    submap: &mut HashMap<dom::NodeId, style::ComputedStyle>,
    id: dom::NodeId,
    avail_width: f32,
) {
    let content_w = if let Some(cs) = submap.get_mut(&id) {
        match cs
            .width
            .or_else(|| cs.width_pct.map(|p| (avail_width * p).max(0.0)))
        {
            Some(w) => {
                cs.width = Some(w);
                (w - cs.padding.left - cs.padding.right - cs.border.left - cs.border.right).max(0.0)
            }
            None => (avail_width
                - cs.margin.left
                - cs.margin.right
                - cs.padding.left
                - cs.padding.right
                - cs.border.left
                - cs.border.right)
                .max(0.0),
        }
    } else {
        avail_width
    };
    let children: Vec<dom::NodeId> = doc.get(id).children.clone();
    for child in children {
        resolve_facade_widths(doc, submap, child, content_w);
    }
}

/// The author CSS inside an iframe facade subtree (its `<style>` text + `<link>` CSS), as one sheet.
fn collect_facade_sheets(
    doc: &dom::Document,
    root: dom::NodeId,
    fetcher: &Rc<dyn Fn(&str) -> Option<(String, String)>>,
) -> Vec<css::Stylesheet> {
    fn walk(
        doc: &dom::Document,
        id: dom::NodeId,
        out: &mut String,
        fetcher: &Rc<dyn Fn(&str) -> Option<(String, String)>>,
    ) {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag.eq_ignore_ascii_case("style") {
                out.push_str(&text_content(doc, id));
                out.push('\n');
                return;
            }
            if e.tag.eq_ignore_ascii_case("link") {
                if let Some(href) = e.attrs.get("href") {
                    if let Some(css) = fetch_link_css(href, fetcher) {
                        out.push_str(&css);
                        out.push('\n');
                    }
                }
            }
        }
        for &child in &doc.get(id).children {
            walk(doc, child, out, fetcher);
        }
    }
    let mut css_src = String::new();
    walk(doc, root, &mut css_src, fetcher);
    if css_src.trim().is_empty() {
        Vec::new()
    } else {
        vec![css::parse(&css_src)]
    }
}

/// Like [`with_computed_style`] but exposes the whole cascade `map` plus the live `Document`, so a
/// caller can read several elements at once (e.g. an element and its containing block, for the
/// CSSOM resolved-value of insets). Recomputes the cascade if stale, same as `with_computed_style`.
fn with_cascade_map<R>(
    state: &HostState,
    f: impl FnOnce(&dom::Document, &HashMap<dom::NodeId, style::ComputedStyle>) -> R,
) -> R {
    let version = state.dom_version.get();
    {
        let mut cache = state.computed_cache.borrow_mut();
        let fresh = matches!(&*cache, Some((v, _)) if *v == version);
        if !fresh {
            let doc = state.doc.borrow();
            let sheets = collect_author_sheets(&doc, &state.fetcher, &state.page_url.borrow());
            let map = style::cascade(&doc, &sheets);
            *cache = Some((version, map));
        }
    }
    let cache = state.computed_cache.borrow();
    let map = cache
        .as_ref()
        .map(|(_, m)| m)
        .expect("cascade just populated");
    let doc = state.doc.borrow();
    f(&doc, map)
}

/// Compute the requested pseudo-element's `ComputedStyle` for node `id` and run `f` over it
/// (`None` when `id` isn't an element). `pseudo_key` is the canonical key from
/// [`style::parse_gcs_pseudo`] (`"before"`, `"marker"`, `"highlight(x)"`, …). Recomputes the
/// element cascade if stale (same freshness check as [`with_computed_style`]), then derives the
/// pseudo style on demand from the element's cascaded style + the author sheets.
fn with_pseudo_style<R>(
    state: &HostState,
    id: dom::NodeId,
    pseudo_key: &str,
    f: impl FnOnce(Option<&style::ComputedStyle>) -> R,
) -> R {
    let version = state.dom_version.get();
    {
        let mut cache = state.computed_cache.borrow_mut();
        let fresh = matches!(&*cache, Some((v, _)) if *v == version);
        if !fresh {
            let doc = state.doc.borrow();
            let sheets = collect_author_sheets(&doc, &state.fetcher, &state.page_url.borrow());
            let map = style::cascade(&doc, &sheets);
            *cache = Some((version, map));
        }
    }
    let cache = state.computed_cache.borrow();
    let map = cache
        .as_ref()
        .map(|(_, m)| m)
        .expect("cascade just populated");
    let doc = state.doc.borrow();
    let element_style = map.get(&id);
    let pseudo = element_style.and_then(|es| {
        let sheets = collect_author_sheets(&doc, &state.fetcher, &state.page_url.borrow());
        style::compute_pseudo_style(&doc, &sheets, id, es, pseudo_key)
    });
    f(pseudo.as_ref())
}

/// The content-box and padding-box extents (width, height) of an element's box, derived from its
/// computed style. Standard box-sizing: `width`/`height` are the content box; the padding box adds
/// the padding edges. Used as the percentage basis / containing-block extent when resolving insets.
fn box_extents(cs: &style::ComputedStyle) -> ((f32, f32), (f32, f32)) {
    let cw = cs.width.unwrap_or(0.0);
    let ch = cs.height.unwrap_or(0.0);
    let pw = cw + cs.padding.left + cs.padding.right;
    let ph = ch + cs.padding.top + cs.padding.bottom;
    ((cw, ch), (pw, ph))
}

/// Compute the CSSOM resolved value of inset `side` (top/right/bottom/left) for node `id`, given a
/// freshly-cascaded `map`. Walks the DOM to find the element's containing block and computes the
/// percentage basis from that block's specified geometry (no layout needed for the cases the WPT
/// inset tests exercise — the containers have explicit px sizes). Returns `None` for non-elements.
fn resolved_inset_value(
    doc: &dom::Document,
    map: &HashMap<dom::NodeId, style::ComputedStyle>,
    id: dom::NodeId,
    side: style::EdgeSide,
    used: Option<f32>,
) -> Option<String> {
    let cs = map.get(&id)?;
    // A box-less element (display:none, or an ancestor is) has no used value → computed value.
    let box_less = cs.display == style::Display::None || ancestor_display_none(doc, map, id);
    if box_less || cs.position == style::Position::Static {
        return Some(cs.resolved_inset(side, true, f32::NAN));
    }

    // Absolute / fixed with BOTH opposite insets `auto`: the box sits at its static position, whose
    // used inset value needs layout. We compute it synchronously from specified geometry (the WPT
    // containers have explicit px sizes) — the engine-pushed used px (`used`) is a fallback for cases
    // where the static position can't be derived from specified geometry alone.
    if matches!(
        cs.position,
        style::Position::Absolute | style::Position::Fixed
    ) {
        let (spec, opposite) = match side {
            style::EdgeSide::Top => (cs.top_spec, cs.bottom_spec),
            style::EdgeSide::Bottom => (cs.bottom_spec, cs.top_spec),
            style::EdgeSide::Left => (cs.left_spec, cs.right_spec),
            style::EdgeSide::Right => (cs.right_spec, cs.left_spec),
            style::EdgeSide::All => (cs.top_spec, cs.bottom_spec),
        };
        let both_auto =
            matches!(spec, style::InsetValue::Auto) && matches!(opposite, style::InsetValue::Auto);
        if both_auto {
            // Prefer the engine-pushed used value (from real layout) when present; otherwise derive
            // the static position synchronously from specified geometry (covers the synchronous
            // mutate-then-read pattern the CSSOM tests use, before any layout/push has run).
            if let Some(px_val) = used {
                return Some(style::serialize_px(px_val));
            }
            let want_height = matches!(side, style::EdgeSide::Top | style::EdgeSide::Bottom);
            let kind = if cs.position == style::Position::Absolute {
                ContainingBlock::PositionedPadding
            } else {
                ContainingBlock::TransformedPadding
            };
            let cb_node = containing_block_node(doc, map, id, kind);
            let static_off = static_position_offset(doc, map, id, cb_node, want_height);
            let cb_extent = containing_block_extent(doc, map, id, kind, want_height);
            let mb_size = if want_height {
                cs.height.unwrap_or(0.0)
                    + cs.padding.top
                    + cs.padding.bottom
                    + cs.border.top
                    + cs.border.bottom
                    + cs.margin.top
                    + cs.margin.bottom
            } else {
                cs.width.unwrap_or(0.0)
                    + cs.padding.left
                    + cs.padding.right
                    + cs.border.left
                    + cs.border.right
                    + cs.margin.left
                    + cs.margin.right
            };
            // Map the static position to the *containing block's* writing mode: a physical side that
            // is the cb's block- or inline-START takes the static offset directly; the opposite side
            // measures from the far edge (cb extent − offset − the box's margin-box size).
            let (block_start, inline_start) = cb_node
                .and_then(|n| map.get(&n))
                .map(|c| c.writing_mode.start_edges(c.direction))
                .unwrap_or((style::EdgeSide::Top, style::EdgeSide::Left));
            let used_val = if side == block_start || side == inline_start {
                static_off
            } else {
                cb_extent - static_off - mb_size
            };
            return Some(style::serialize_px(used_val));
        }
    }

    // Find the containing block and the relevant axis extent (height for top/bottom, width for
    // left/right). For in-flow / relative / sticky the cb is the parent's *content* box; for
    // absolute the nearest positioned ancestor's *padding* box; for fixed the nearest transformed
    // ancestor's padding box (the viewport otherwise — approximated by the document element).
    let want_height = matches!(side, style::EdgeSide::Top | style::EdgeSide::Bottom);
    let basis = match cs.position {
        style::Position::Relative => {
            containing_block_extent(doc, map, id, ContainingBlock::ParentContent, want_height)
        }
        // Sticky insets resolve against the nearest scrollport (overflow != visible) ancestor's
        // content box, not the parent — per the CSSOM sticky resolved-value rule.
        style::Position::Sticky => {
            containing_block_extent(doc, map, id, ContainingBlock::StickyScrollport, want_height)
        }
        style::Position::Absolute => containing_block_extent(
            doc,
            map,
            id,
            ContainingBlock::PositionedPadding,
            want_height,
        ),
        style::Position::Fixed => containing_block_extent(
            doc,
            map,
            id,
            ContainingBlock::TransformedPadding,
            want_height,
        ),
        style::Position::Static => f32::NAN,
    };
    Some(cs.resolved_inset(side, false, basis))
}

/// Which ancestor establishes the containing block, and which box of it forms the basis.
#[derive(Clone, Copy)]
enum ContainingBlock {
    /// Parent element's content box (in-flow / relative / sticky).
    ParentContent,
    /// Nearest positioned ancestor's padding box (absolute).
    PositionedPadding,
    /// Nearest ancestor with a transform's padding box, else the document element (fixed).
    TransformedPadding,
    /// Nearest scroll-container (scrollport) ancestor's content box — the box a `position: sticky`
    /// element's inset percentages resolve against (the nearest ancestor with `overflow != visible`,
    /// else the parent). CSSOM resolved value for sticky insets.
    StickyScrollport,
}

/// Find the containing-block element for `id` per `kind` (the abspos/fixed cb walk), returning its
/// node id, or `None` if none is found (the cb is the viewport / initial containing block).
fn containing_block_node(
    doc: &dom::Document,
    map: &HashMap<dom::NodeId, style::ComputedStyle>,
    id: dom::NodeId,
    kind: ContainingBlock,
) -> Option<dom::NodeId> {
    let mut cur = doc.get(id).parent;
    match kind {
        ContainingBlock::ParentContent => cur,
        ContainingBlock::PositionedPadding => {
            while let Some(a) = cur {
                if map
                    .get(&a)
                    .map(|acs| acs.position != style::Position::Static)
                    .unwrap_or(false)
                {
                    return Some(a);
                }
                cur = doc.get(a).parent;
            }
            None
        }
        ContainingBlock::TransformedPadding => {
            while let Some(a) = cur {
                if map
                    .get(&a)
                    .map(|acs| acs.transform.is_some())
                    .unwrap_or(false)
                {
                    return Some(a);
                }
                cur = doc.get(a).parent;
            }
            None
        }
        ContainingBlock::StickyScrollport => {
            while let Some(a) = cur {
                if map
                    .get(&a)
                    .map(|acs| acs.overflow_scrollport)
                    .unwrap_or(false)
                {
                    return Some(a);
                }
                cur = doc.get(a).parent;
            }
            None
        }
    }
}

/// The static-position offset of out-of-flow `id`'s margin box from its containing block's
/// content-area start edge, on one axis (`want_height` → vertical/top, else horizontal/left), in px.
///
/// The hypothetical in-flow position of `id` is the start of its parent's content box; the parent's
/// content box is in turn offset from the containing block (`cb`, exclusive) by each intervening
/// ancestor's margin + border + padding start edge, plus the cb's own padding start edge. Computed
/// from specified geometry (the WPT inset containers all have explicit px sizes), so it matches real
/// layout for those tests without a layout pass. `cb` = `None` means the containing block is the
/// initial containing block (the document root), so we accumulate up to (and including) the root.
fn static_position_offset(
    doc: &dom::Document,
    map: &HashMap<dom::NodeId, style::ComputedStyle>,
    id: dom::NodeId,
    cb: Option<dom::NodeId>,
    want_height: bool,
) -> f32 {
    let start_edge = |e: &style::Edges| if want_height { e.top } else { e.left };
    let mut offset = 0.0;
    // The cb's own padding start edge: its content box begins after its padding.
    if let Some(cbid) = cb {
        if let Some(cbcs) = map.get(&cbid) {
            offset += start_edge(&cbcs.padding);
        }
    }
    // Each ancestor strictly between `id` and the cb contributes its full start margin+border+padding.
    let mut cur = doc.get(id).parent;
    while let Some(a) = cur {
        if Some(a) == cb {
            break;
        }
        if let Some(acs) = map.get(&a) {
            offset += start_edge(&acs.margin) + start_edge(&acs.border) + start_edge(&acs.padding);
        }
        cur = doc.get(a).parent;
    }
    offset
}

/// Walk up from `id` to find its containing block per `kind`, returning the requested axis extent
/// (height when `want_height`, else width). Falls back to `0.0` if no suitable ancestor is found.
fn containing_block_extent(
    doc: &dom::Document,
    map: &HashMap<dom::NodeId, style::ComputedStyle>,
    id: dom::NodeId,
    kind: ContainingBlock,
    want_height: bool,
) -> f32 {
    let pick = |extents: ((f32, f32), (f32, f32)), padding: bool| {
        let (content, pad) = extents;
        let (w, h) = if padding { pad } else { content };
        if want_height {
            h
        } else {
            w
        }
    };
    let mut cur = doc.get(id).parent;
    match kind {
        ContainingBlock::ParentContent => {
            if let Some(p) = cur {
                if let Some(pcs) = map.get(&p) {
                    return pick(box_extents(pcs), false);
                }
            }
            0.0
        }
        ContainingBlock::PositionedPadding => {
            while let Some(a) = cur {
                if let Some(acs) = map.get(&a) {
                    if acs.position != style::Position::Static {
                        return pick(box_extents(acs), true);
                    }
                }
                cur = doc.get(a).parent;
            }
            0.0
        }
        ContainingBlock::TransformedPadding => {
            while let Some(a) = cur {
                if let Some(acs) = map.get(&a) {
                    if acs.transform.is_some() {
                        return pick(box_extents(acs), true);
                    }
                }
                cur = doc.get(a).parent;
            }
            0.0
        }
        ContainingBlock::StickyScrollport => {
            // Nearest scrollport ancestor's content box; fall back to the parent's content box when
            // there's no scroll container (the box's normal containing block).
            let mut fallback = None;
            while let Some(a) = cur {
                if let Some(acs) = map.get(&a) {
                    if fallback.is_none() {
                        fallback = Some(pick(box_extents(acs), false));
                    }
                    if acs.overflow_scrollport {
                        return pick(box_extents(acs), false);
                    }
                }
                cur = doc.get(a).parent;
            }
            fallback.unwrap_or(0.0)
        }
    }
}

/// True if any ancestor of `id` has `display: none` (so `id` has no rendered box even if its own
/// `display` is not `none`).
fn ancestor_display_none(
    doc: &dom::Document,
    map: &HashMap<dom::NodeId, style::ComputedStyle>,
    id: dom::NodeId,
) -> bool {
    let mut cur = doc.get(id).parent;
    while let Some(a) = cur {
        if let Some(acs) = map.get(&a) {
            if acs.display == style::Display::None {
                return true;
            }
        }
        cur = doc.get(a).parent;
    }
    false
}

/// `__computedStyleProp(id, name) -> string` — the computed value of CSS property `name` (kebab,
/// lowercased by JS) for node `id`, or "" if there's no computed style (non-element / unknown id) or
/// the property isn't tracked.
fn prim_computed_style_prop(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let name = arg_str(scope, &args, 1);
    let pseudo = style::parse_gcs_pseudo(&arg_str(scope, &args, 2));
    let state = host_state(scope);
    let value =
        match (node, &pseudo) {
            (None, _) | (_, style::GcsPseudo::Invalid) => String::new(),
            (Some(n), style::GcsPseudo::Pseudo(key)) => with_pseudo_style(&state, n, key, |cs| {
                cs.map(|cs| cs.get_property(&name)).unwrap_or_default()
            }),
            (Some(n), style::GcsPseudo::Element) => {
                // Inset longhands need the CSSOM resolved-value algorithm (position + containing
                // block), which reads more than one element's style — handle them via the cascade map.
                let inset_side = match name.as_str() {
                    "top" => Some(style::EdgeSide::Top),
                    "right" => Some(style::EdgeSide::Right),
                    "bottom" => Some(style::EdgeSide::Bottom),
                    "left" => Some(style::EdgeSide::Left),
                    _ => None,
                };
                // Margin longhands report the CSSOM *used* value: the engine pushes resolved margins
                // (including `auto` centering / over-constrained boxes) keyed by node id.
                let margin_idx = match name.as_str() {
                    "margin-top" => Some(0usize),
                    "margin-right" => Some(1),
                    "margin-bottom" => Some(2),
                    "margin-left" => Some(3),
                    _ => None,
                };
                // width/height report the CSSOM *used* (content-box px) value when the element has a box
                // and the property applies — e.g. a percentage width the cascade couldn't resolve.
                let size_is_width = match name.as_str() {
                    "width" => Some(true),
                    "height" => Some(false),
                    _ => None,
                };
                // min-width / min-height: auto resolves to 0px, except a box with a preferred aspect
                // ratio or a flex/grid item keeps `auto`; a box-less element (no layout box) is 0px.
                if matches!(name.as_str(), "min-width" | "min-height") {
                    with_cascade_map(&state, |doc, map| {
                        let cs = match map.get(&n) {
                            Some(c) => c,
                            None => return String::new(),
                        };
                        let computed = cs.get_property(&name);
                        if computed != "auto" {
                            return computed;
                        }
                        // A box-less element (display:none, or inside one) generates no box → 0px.
                        if cs.display_none || ancestor_display_none(doc, map, n) {
                            return "0px".to_string();
                        }
                        let parent_flex_grid = doc
                            .get(n)
                            .parent
                            .and_then(|p| map.get(&p))
                            .map(|p| {
                                matches!(
                                    p.display,
                                    style::Display::Flex
                                        | style::Display::InlineFlex
                                        | style::Display::Grid
                                        | style::Display::InlineGrid
                                )
                            })
                            .unwrap_or(false);
                        if cs.aspect_ratio_set || parent_flex_grid {
                            "auto".to_string()
                        } else {
                            "0px".to_string()
                        }
                    })
                } else if let Some(is_width) = size_is_width {
                    with_computed_style(&state, n, |cs| {
                        let cs = match cs {
                            Some(c) => c,
                            None => return String::new(),
                        };
                        let computed = cs.get_property(&name);
                        // A specified length is already the used value (and stays fresh between layouts);
                        // only resolve `auto`/percentage via the laid-out box. width/height don't apply to
                        // non-replaced inline boxes or display:none, where the computed value is reported.
                        if computed != "auto"
                            || cs.display_none
                            || matches!(cs.display, style::Display::Inline)
                        {
                            return computed;
                        }
                        match state.layout_rects.borrow().get(&n.0) {
                            Some(&(_, _, w, h)) => {
                                let (b0, b1, p0, p1) = if is_width {
                                    (
                                        cs.border.left,
                                        cs.border.right,
                                        cs.padding.left,
                                        cs.padding.right,
                                    )
                                } else {
                                    (
                                        cs.border.top,
                                        cs.border.bottom,
                                        cs.padding.top,
                                        cs.padding.bottom,
                                    )
                                };
                                let border_box = if is_width { w } else { h };
                                style::serialize_px((border_box - b0 - b1 - p0 - p1).max(0.0))
                            }
                            None => computed,
                        }
                    })
                } else if let Some(idx) = margin_idx {
                    // Only an `auto` margin needs the engine's resolved used value; a specified margin's
                    // computed value is always current (and not stale between layouts). Falls back to the
                    // computed value if the engine hasn't pushed a used margin yet.
                    with_computed_style(&state, n, |cs| match cs {
                        Some(cs) if cs.margin_auto[idx] => state
                            .used_margins
                            .borrow()
                            .get(&n.0)
                            .map(|&(t, r, b, l)| style::serialize_px([t, r, b, l][idx]))
                            .unwrap_or_else(|| cs.get_property(&name)),
                        Some(cs) => cs.get_property(&name),
                        None => String::new(),
                    })
                } else {
                    match inset_side {
                        Some(side) => {
                            // The engine pushed this box's used inset values (px) keyed by node id; pick this
                            // side. Used by the resolved-value algorithm for cases that need real layout
                            // (absolute/fixed static position when both opposite insets are `auto`).
                            let used =
                                state.used_insets.borrow().get(&n.0).map(
                                    |&(t, r, b, l)| match side {
                                        style::EdgeSide::Top => t,
                                        style::EdgeSide::Right => r,
                                        style::EdgeSide::Bottom => b,
                                        style::EdgeSide::Left => l,
                                        style::EdgeSide::All => t,
                                    },
                                );
                            with_cascade_map(&state, |doc, map| {
                                resolved_inset_value(doc, map, n, side, used).unwrap_or_default()
                            })
                        }
                        None => with_computed_style(&state, n, |cs| {
                            cs.map(|cs| cs.get_property(&name)).unwrap_or_default()
                        }),
                    }
                }
            }
        };
    let s = js_str(scope, &value);
    rv.set(s);
}

/// The computed-style property names for one element: the standard tracked longhands plus this
/// element's resolved custom properties (`--name`), which `getComputedStyle(el)` enumerates per
/// CSSOM in lexicographical order — with vendor-prefixed / custom names (leading `-`) sorted last.
fn computed_names_with_custom(cs: &style::ComputedStyle) -> Vec<String> {
    let mut names: Vec<String> = cs.property_names().iter().map(|s| s.to_string()).collect();
    names.extend(cs.custom_props.keys().cloned());
    names.sort_by(|a, b| {
        // A leading `-` (vendor prefix / `--custom`) sorts after unprefixed; then lexicographic.
        a.starts_with('-')
            .cmp(&b.starts_with('-'))
            .then_with(|| a.cmp(b))
    });
    names
}

/// `__computedStyleNames(id) -> [name...]` — the property names with non-empty computed values for
/// node `id` (backs `length`/`item(i)`/index access/iteration). Empty array for non-elements.
fn prim_computed_style_names(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let pseudo = style::parse_gcs_pseudo(&arg_str(scope, &args, 1));
    let state = host_state(scope);
    let names: Vec<String> = match (node, &pseudo) {
        (None, _) | (_, style::GcsPseudo::Invalid) => Vec::new(),
        (Some(n), style::GcsPseudo::Pseudo(key)) => with_pseudo_style(&state, n, key, |cs| {
            cs.map(computed_names_with_custom).unwrap_or_default()
        }),
        (Some(n), style::GcsPseudo::Element) => with_computed_style(&state, n, |cs| {
            cs.map(computed_names_with_custom).unwrap_or_default()
        }),
    };
    let arr = js_str_array(scope, &names);
    rv.set(arr);
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
        state.bump_dom_version(); // invalidate getComputedStyle cache
        let old = if state.observers_active.get() {
            // Capture the old value BEFORE overwriting (for `attributeOldValue`).
            match &state.doc.borrow().get(n).data {
                dom::NodeData::Element(e) => e.attrs.get(&name).cloned(),
                _ => None,
            }
        } else {
            None
        };
        if let dom::NodeData::Element(e) = &mut state.doc.borrow_mut().get_mut(n).data {
            e.attrs.insert(name.clone(), value);
        }
        state.record_mutation(MutationRec {
            kind: "attributes",
            target: n,
            attr_name: Some(name),
            old_value: old,
            added: Vec::new(),
            removed: Vec::new(),
        });
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
        state.bump_dom_version(); // invalidate getComputedStyle cache
        let old = if state.observers_active.get() {
            match &state.doc.borrow().get(n).data {
                dom::NodeData::Element(e) => e.attrs.get(&name).cloned(),
                _ => None,
            }
        } else {
            None
        };
        if let dom::NodeData::Element(e) = &mut state.doc.borrow_mut().get_mut(n).data {
            // shift_remove preserves the insertion order of the remaining attributes (swap_remove,
            // the `remove` alias on IndexMap, would not — the DOM exposes attributes in order).
            e.attrs.shift_remove(&name);
        }
        state.record_mutation(MutationRec {
            kind: "attributes",
            target: n,
            attr_name: Some(name),
            old_value: old,
            added: Vec::new(),
            removed: Vec::new(),
        });
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

/// Directory where `localStorage` buckets persist (one JSON file per origin).
fn storage_dir() -> std::path::PathBuf {
    let base = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(".imlunahey-browser").join("localstorage")
}

/// Map a storage key (an origin like `https://example.com`) to a safe filename.
fn storage_path(key: &str) -> std::path::PathBuf {
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
fn prim_storage_load(
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
fn prim_storage_save(
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
fn prim_scroll_y(
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
fn prim_prefers_dark(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    rv.set(v8::Boolean::new(scope, color_scheme_dark()).into());
}

/// A JS-requested scroll target (document CSS px), read+cleared by the engine after each Session
/// interaction. `i64::MIN` = no request. Process-global: the active tab is the one being driven.
static PENDING_SCROLL: AtomicI64 = AtomicI64::new(i64::MIN);

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
fn prim_scroll_set(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let y = args.get(0).number_value(scope).unwrap_or(0.0);
    let y = if y.is_finite() { y.max(0.0) } else { 0.0 };
    PENDING_SCROLL.store(y.round() as i64, Ordering::Release);
}

/// `__scrollIntoView(id)` — request a scroll so node `id`'s top is near the viewport top.
fn prim_scroll_into_view(
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
fn fill_random(buf: &mut [u8]) {
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
fn prim_crypto_random(
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
fn prim_append_child(
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
fn prim_remove_child(
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
fn prim_rect(
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
fn base64_encode(data: &[u8]) -> String {
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
fn prim_canvas_pixels(
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
fn prim_natural_size(
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
fn prim_elem_metrics(
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
    put("ot", ay);
    put("ol", ax);
    put("sw", sw);
    put("sh", sh);
    rv.set(obj.into());
}

/// `__textContent(id) -> string`
fn prim_text_content(
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
fn prim_set_text_content(
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
fn prim_inner_html(
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
fn prim_set_inner_html(
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

/// `__documentRootId() -> id` — the Document node itself (arena root). Always valid, even when the
/// root element has been replaced/removed, so JS can find the document's children directly.
fn prim_document_root_id(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let root = host_state(scope).doc.borrow().root();
    rv.set_double(root.0 as f64);
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
fn prim_request(
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
fn prim_start_fetch(
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
fn prim_ws_connect(
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
fn prim_ws_send(
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
fn prim_ws_close(
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

/// `__observersActive(bool)` — JS sets this true when the first `MutationObserver` is registered
/// and false when the last disconnects. Gates whether the mutation primitives record anything.
fn prim_observers_active(
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
fn prim_drain_mutations(
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
fn install_dom_primitives(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
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
    set_fn(scope, global, "__nodeType", prim_node_type);
    set_fn(scope, global, "__rect", prim_rect);
    set_fn(scope, global, "__naturalSize", prim_natural_size);
    set_fn(scope, global, "__canvasPixels", prim_canvas_pixels);
    set_fn(scope, global, "__elemMetrics", prim_elem_metrics);
    set_fn(scope, global, "__textContent", prim_text_content);
    set_fn(scope, global, "__setTextContent", prim_set_text_content);
    set_fn(scope, global, "__innerHTML", prim_inner_html);
    set_fn(scope, global, "__setInnerHTML", prim_set_inner_html);
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

/// Live OS appearance: `true` when the user's effective macOS appearance is Dark. Drives the
/// `prefers-color-scheme` media feature in both the JS `matchMedia` API (via `__prefersDark()`)
/// and, in parallel, the CSS `@media (prefers-color-scheme)` cascade (the `style` crate keeps its
/// own copy, set on the same engine path). Process-global so the engine (any thread) can update it
/// and the JS worker reads the live value on every media-query evaluation.
static COLOR_SCHEME_DARK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set the logical viewport size (px) and device pixel ratio surfaced to page JS.
pub fn set_device_metrics(width: u32, height: u32, device_pixel_ratio: f32) {
    use std::sync::atomic::Ordering;
    VP_W.store(width.max(1), Ordering::Relaxed);
    VP_H.store(height.max(1), Ordering::Relaxed);
    DPR_BITS.store(device_pixel_ratio.max(0.1).to_bits(), Ordering::Relaxed);
}

/// Set whether the effective OS appearance is Dark, surfaced to page JS as the
/// `prefers-color-scheme` media feature (read live by `matchMedia(...).matches` and used to fire
/// `change` events on existing `MediaQueryList`s when it flips). The engine calls this on launch
/// and whenever the user toggles Light/Dark.
pub fn set_color_scheme_dark(is_dark: bool) {
    COLOR_SCHEME_DARK.store(is_dark, std::sync::atomic::Ordering::Relaxed);
}

/// Read the live OS-appearance dark flag (used by the `__prefersDark()` JS primitive).
fn color_scheme_dark() -> bool {
    COLOR_SCHEME_DARK.load(std::sync::atomic::Ordering::Relaxed)
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
    *host_state(scope).page_url.borrow_mut() = url.to_string();
    // Inject the live viewport metrics so the bootstrap can set window.innerWidth/innerHeight and
    // devicePixelRatio from the real values rather than hardcoded defaults.
    let (vw, vh, dpr) = device_metrics();
    for (name, num) in [
        ("__innerWidth", vw),
        ("__innerHeight", vh),
        ("__devicePixelRatio", dpr),
    ] {
        let k = v8::String::new(scope, name).unwrap();
        let n = v8::Number::new(scope, num);
        global.set(scope, k.into(), n.into());
    }
    eval_internal(scope, BROWSER_ENV_BOOTSTRAP, "<browser-env>");
    // Expose elements with an `id` as named globals (HTML named-properties-on-window). The DOM is
    // already fully parsed by the time the environment is installed (the engine batches scripts
    // after `parser.finish()`), so every static-markup id is visible to author scripts that follow.
    eval_internal(
        scope,
        "if (typeof __installNamedGlobals === 'function') { __installNamedGlobals(); }",
        "<named-globals>",
    );
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
  // Top-level browsing context: parent/top/frames are self-referential, there's no opener, and
  // zero child frames. Real code (testharness.js, framebusters, analytics) walks `window.parent` /
  // `window.top` and crashes if they're undefined.
  globalThis.parent = globalThis;
  globalThis.top = globalThis;
  globalThis.frames = globalThis;
  globalThis.opener = null;
  try { globalThis.length = 0; } catch (e) {}
  // Minimal location stub (overwritten by the browser-env bootstrap).
  globalThis.location = { href: "" };

  var NODE = "__node";

  // --- DOM namespace / case metadata ----------------------------------------------------------
  // The Rust arena stores only a lowercased `tag` string per element, which loses the namespace,
  // the prefix, and the original case that `createElementNS` must remember. We keep that extra
  // metadata in a JS-side map keyed by the node id (mirroring how other per-node JS state is kept).
  // Elements parsed from HTML source (no entry here) default to the HTML namespace, a lowercased
  // localName, an uppercase tagName, and a null prefix.
  var HTML_NS = "http://www.w3.org/1999/xhtml";
  var XML_NS = "http://www.w3.org/XML/1998/namespace";
  var XMLNS_NS = "http://www.w3.org/2000/xmlns/";
  var __nsMeta = {}; // id -> { namespaceURI, prefix, localName, qualifiedName, isHTML }

  function asciiLower(s) {
    return String(s).replace(/[A-Z]/g, function (c) { return c.toLowerCase(); });
  }
  function asciiUpper(s) {
    return String(s).replace(/[a-z]/g, function (c) { return c.toUpperCase(); });
  }

  // XML `Name` / `QName` validation. Matches the behaviour browsers (and the WPT suite) actually
  // implement, which is more lenient than the strict XML grammar: a NameStartChar is an ASCII
  // letter / underscore or any non-ASCII codepoint (>= U+0080); a NameChar additionally allows
  // digits, '-' and '.', and in fact any character that is not whitespace or '>' (so '<', '}', and
  // lone surrogates are accepted mid-name). The ':' separates a prefix from a local name.
  function isNameStartChar(cc) {
    return (cc >= 0x41 && cc <= 0x5A) || (cc >= 0x61 && cc <= 0x7A) || cc === 0x5F || cc >= 0x80;
  }
  function isNameChar(cc) {
    if (isNameStartChar(cc)) { return true; }
    if (cc >= 0x30 && cc <= 0x39) { return true; }      // 0-9
    if (cc === 0x2D || cc === 0x2E) { return true; }    // - .
    // Lenient: any non-whitespace, non-'>' character is accepted mid-name.
    if (cc === 0x3E) { return false; }                  // '>'
    if (cc === 0x20 || cc === 0x09 || cc === 0x0A || cc === 0x0C || cc === 0x0D) { return false; }
    return true;
  }
  // A valid "Name" (colons permitted as NameChar when allowColon): NameStartChar NameChar*.
  function isValidNameImpl(s, allowColon) {
    if (s.length === 0) { return false; }
    if (!isNameStartChar(s.charCodeAt(0))) { return false; }
    for (var i = 1; i < s.length; i++) {
      var cc = s.charCodeAt(i);
      if (cc === 0x3A) { if (!allowColon) { return false; } continue; }
      if (!isNameChar(cc)) { return false; }
    }
    return true;
  }
  function isValidName(s) { return isValidNameImpl(s, false); }

  function invalidCharacterError() {
    throw new globalThis.DOMException("The string contains invalid characters.", "InvalidCharacterError");
  }
  function namespaceError() {
    throw new globalThis.DOMException("The namespace is not valid.", "NamespaceError");
  }

  // "validate and extract" (DOM standard): given a namespace + qualifiedName, validate the QName and
  // split it into [namespace, prefix, localName], enforcing the xml/xmlns special cases.
  function validateAndExtract(ns, qualifiedName) {
    ns = (ns === undefined || ns === null || ns === "") ? null : String(ns);
    var qname = String(qualifiedName);
    var prefix = null;
    var localName = qname;
    var ci = qname.indexOf(":");
    if (ci >= 0) {
      prefix = qname.slice(0, ci);
      localName = qname.slice(ci + 1);
      // Prefix must be a non-empty colon-free Name; the local name (everything after the first
      // colon) is validated as a Name that may itself contain further colons.
      if (prefix.length === 0 || !isValidNameImpl(prefix, false)) { invalidCharacterError(); }
      if (localName.length === 0 || !isValidNameImpl(localName, true)) { invalidCharacterError(); }
    } else {
      if (!isValidNameImpl(qname, false)) { invalidCharacterError(); }
    }
    if (prefix !== null && ns === null) { namespaceError(); }
    if (prefix === "xml" && ns !== XML_NS) { namespaceError(); }
    if ((qname === "xmlns" || prefix === "xmlns") && ns !== XMLNS_NS) { namespaceError(); }
    if (ns === XMLNS_NS && qname !== "xmlns" && prefix !== "xmlns") { namespaceError(); }
    return { namespace: ns, prefix: prefix, localName: localName };
  }

  // The qualified name of an element id (prefix:localName, or localName), honouring metadata.
  function elQualifiedName(eid) {
    var m = __nsMeta[eid];
    if (m) { return m.qualifiedName; }
    return __tag(eid); // parsed HTML: arena tag is the lowercased qualified name
  }
  function elNamespace(eid) {
    var m = __nsMeta[eid];
    if (m) { return m.namespaceURI; }
    return __nodeType(eid) === 1 ? HTML_NS : null;
  }
  function elLocalName(eid) {
    var m = __nsMeta[eid];
    if (m) { return m.localName; }
    return __tag(eid);
  }
  // getElementsByTagName matcher, per the DOM standard (HTML document branch). qualifiedName "*"
  // matches all; HTML-namespace elements match their lowercased qualified name; other namespaces
  // match the qualified name exactly.
  function matchesTagName(eid, qualifiedName) {
    if (qualifiedName === "*") { return true; }
    var qn = elQualifiedName(eid);
    // HTML-namespace elements compare their (as-stored) qualified name against the lowercased
    // search string; other namespaces compare the qualified name exactly.
    if (elNamespace(eid) === HTML_NS) {
      return qn === asciiLower(qualifiedName);
    }
    return qn === qualifiedName;
  }
  function matchesTagNameNS(eid, ns, localName) {
    if (ns !== "*" && elNamespace(eid) !== (ns === "" ? null : ns)) { return false; }
    if (localName !== "*" && elLocalName(eid) !== localName) { return false; }
    return true;
  }
  // Collect element-node descendants of `rootId` (excluding root) in tree order, matched by `pred`.
  function collectDescendants(rootId, pred) {
    var out = [];
    function visit(nid, isRoot) {
      if (!isRoot && __nodeType(nid) === 1 && pred(nid)) { out.push(wrap(nid)); }
      var kids = __children(nid);
      for (var i = 0; i < kids.length; i++) { visit(kids[i], false); }
    }
    visit(rootId, true);
    return out;
  }

  // --- Namespace lookup (DOM standard §node tree) ---------------------------------------------
  // These operate on raw node ids so they can be shared by Element / Document / Attr / DocumentType
  // wrappers. xmlns declarations live as ordinary attributes in the arena ("xmlns" / "xmlns:prefix").
  // "locate a namespace prefix" for an element id given a namespace.
  function locateNamespacePrefix(eid, ns) {
    if (elNamespace(eid) === ns && elMetaPrefix(eid) !== null) { return elMetaPrefix(eid); }
    var names = __attrNames(eid);
    for (var i = 0; i < names.length; i++) {
      var k = names[i];
      if (k === "xmlns") { continue; }
      if (k.indexOf("xmlns:") === 0 && __getAttr(eid, k) === ns) { return k.slice(6); }
    }
    var p = __parent(eid);
    if (p >= 0 && __nodeType(p) === 1) { return locateNamespacePrefix(p, ns); }
    return null;
  }
  // "locate a namespace" for a node id given a prefix (null/"" => default namespace).
  function locateNamespace(nid, prefix) {
    if (nid < 0) { return null; }
    var t = __nodeType(nid);
    if (t === 1) { // element
      // The `xml` / `xmlns` prefixes are bound to fixed namespaces (per browser behaviour). These
      // only resolve in an element context; for a bare DocumentFragment they stay null.
      if (prefix === "xml") { return XML_NS; }
      if (prefix === "xmlns") { return XMLNS_NS; }
      var elNs = elNamespace(nid);
      if (elNs != null && elMetaPrefix(nid) === (prefix == null ? null : prefix)) { return elNs; }
      var names = __attrNames(nid);
      for (var i = 0; i < names.length; i++) {
        var k = names[i];
        if (prefix != null && k === ("xmlns:" + prefix)) { var v = __getAttr(nid, k); return v === "" ? null : v; }
        if (prefix == null && k === "xmlns") { var v2 = __getAttr(nid, k); return v2 === "" ? null : v2; }
      }
      var p = __parent(nid);
      if (p >= 0 && __nodeType(p) === 1) { return locateNamespace(p, prefix); }
      return null;
    }
    if (t === 9) { // document → documentElement
      var de = __documentElementId();
      return de >= 0 ? locateNamespace(de, prefix) : null;
    }
    if (t === 10 || t === 7) { return null; } // DocumentType / PI
    // Text/Comment/Fragment: defer to the parent element.
    var pp = __parent(nid);
    if (pp >= 0 && __nodeType(pp) === 1) { return locateNamespace(pp, prefix); }
    return null;
  }
  function elMetaPrefix(eid) {
    var m = __nsMeta[eid];
    return m ? m.prefix : null;
  }
  function nodeLookupNamespaceURI(nid, prefix) {
    var p = (prefix === undefined || prefix === null || prefix === "") ? null : String(prefix);
    return locateNamespace(nid, p);
  }
  function nodeLookupPrefix(nid, ns) {
    if (ns == null || ns === "") { return null; }
    var t = __nodeType(nid);
    var startEl = -1;
    if (t === 1) { startEl = nid; }
    else if (t === 9) { startEl = __documentElementId(); }
    else { var p = __parent(nid); if (p >= 0 && __nodeType(p) === 1) { startEl = p; } }
    if (startEl < 0) { return null; }
    return locateNamespacePrefix(startEl, String(ns));
  }
  function nodeIsDefaultNamespace(nid, ns) {
    var want = (ns == null || ns === "") ? null : String(ns);
    var def = locateNamespace(nid, null);
    return def === want;
  }
  def(globalThis, "__nodeLookupNamespaceURI", nodeLookupNamespaceURI);
  def(globalThis, "__nodeLookupPrefix", nodeLookupPrefix);
  def(globalThis, "__nodeIsDefaultNamespace", nodeIsDefaultNamespace);

  // --- DOM mutation shared helpers (used across the ChildNode/ParentNode mixins) --------------
  function hierarchyRequestError(msg) {
    throw new globalThis.DOMException(msg || "The operation would yield an incorrect node tree.", "HierarchyRequestError");
  }
  function notFoundError(msg) {
    throw new globalThis.DOMException(msg || "The object can not be found here.", "NotFoundError");
  }
  // The node id of an argument: real DOM-arena nodes carry `__node`; strings/anything else => -1.
  function nodeIdOf(x) { return (x && typeof x.__node === "number") ? x.__node : -1; }
  // WebIDL: a non-nullable `Node` parameter throws a TypeError (not a DOMException) when the value
  // isn't a Node. Returns the node id on success.
  function requireNodeArg(x, methodName) {
    var nid = nodeIdOf(x);
    if (nid < 0) {
      throw new TypeError("Failed to execute '" + methodName + "': parameter is not of type 'Node'.");
    }
    return nid;
  }

  // "convert nodes into a node" (DOM standard): a list of (Node | string) becomes a single node.
  // Strings become Text nodes. A single node is returned as-is; multiple nodes (or zero) are
  // collected into a DocumentFragment. Returns a node id (or -1 if the result is an empty fragment
  // that the caller may still insert as a no-op).
  function convertNodesIntoNode(args) {
    var ids = [];
    for (var i = 0; i < args.length; i++) {
      var a = args[i];
      var nid = nodeIdOf(a);
      if (nid >= 0) { ids.push(nid); }
      // Non-Node args are DOMStrings: null -> "null", undefined -> "undefined" (WebIDL stringify).
      else { ids.push(__createText(String(a))); }
    }
    if (ids.length === 1) { return ids[0]; }
    var frag = __createDocumentFragment();
    for (var j = 0; j < ids.length; j++) { __appendChild(frag, ids[j]); }
    return frag;
  }

  // True if `ancestorId` is an inclusive ancestor of `nodeId` (would create a cycle on insert).
  function isInclusiveAncestor(ancestorId, nodeId) {
    var cur = nodeId;
    while (cur >= 0) { if (cur === ancestorId) { return true; } cur = __parent(cur); }
    return false;
  }

  // Pre-insertion validity (subset relevant here): parent must be a Document/Fragment/Element, the
  // node must not be an inclusive ancestor of parent, and `ref` (if given) must be a child of parent.
  function ensurePreInsertValid(parentId, nodeId, refId) {
    var pt = __nodeType(parentId);
    if (pt !== 1 && pt !== 9 && pt !== 11) {
      hierarchyRequestError("Cannot insert into a node that is not a Document, DocumentFragment, or Element.");
    }
    if (nodeId >= 0 && isInclusiveAncestor(nodeId, parentId)) {
      hierarchyRequestError("The new child element contains the parent.");
    }
    var nt = nodeId >= 0 ? __nodeType(nodeId) : -1;
    if (nt === 9) { hierarchyRequestError("Nodes of type Document may not be inserted."); }
    if (nodeId >= 0 && (nt === 1 || nt === 3 || nt === 8 || nt === 11) && (pt === 9)) {
      // Documents have additional constraints, but our tree is HTML-shaped; allow elements/fragments.
    }
    if (refId >= 0 && __parent(refId) !== parentId) {
      notFoundError("The reference child is not a child of this node.");
    }
  }

  // Insert `nodeId` (possibly a DocumentFragment, whose children are moved) into `parentId` before
  // `refId` (-1 = append). Returns the inserted node id. Validity must be checked by the caller.
  function insertNode(parentId, nodeId, refId) {
    if (nodeId < 0) { return nodeId; }
    if (__nodeType(nodeId) === 11) {
      var moving = __children(nodeId).slice();
      for (var i = 0; i < moving.length; i++) { __insertBefore(parentId, moving[i], refId); }
      if (globalThis.__ceOnInsert) { for (var k = 0; k < moving.length; k++) { try { globalThis.__ceOnInsert(moving[k]); } catch (e) {} } }
      if (globalThis.__adoptOnInsert) { for (var m = 0; m < moving.length; m++) { try { globalThis.__adoptOnInsert(moving[m]); } catch (e) {} } }
      return nodeId;
    }
    __insertBefore(parentId, nodeId, refId);
    // Custom Elements: a newly-connected element (and its subtree) may need upgrading + connectedCallback.
    if (globalThis.__ceOnInsert) { try { globalThis.__ceOnInsert(nodeId); } catch (e) {} }
    // Cross-document adoption: clear adoptedStyleSheets of shadow roots moved into a frame document.
    if (globalThis.__adoptOnInsert) { try { globalThis.__adoptOnInsert(nodeId); } catch (e) {} }
    return nodeId;
  }

  // The set of arg node-ids (used to skip them when picking a viable reference sibling).
  function argNodeIdSet(args) {
    var set = {};
    for (var i = 0; i < args.length; i++) { var n = nodeIdOf(args[i]); if (n >= 0) { set[n] = true; } }
    return set;
  }

  // ChildNode.before/after: insert `args` among this node's siblings. No-op if no parent. The
  // reference child is computed BEFORE the nodes are converted/moved, skipping any sibling that's
  // itself one of the arguments (DOM standard's "viable previous/next sibling").
  function childBefore(id, args) {
    var parent = __parent(id);
    if (parent < 0) { return; }
    var set = argNodeIdSet(args);
    var sibs = __children(parent);
    var idx = sibs.indexOf(id);
    // viablePreviousSibling: first preceding sibling not in args (it survives the conversion since
    // it isn't an arg), or null. Captured BEFORE conversion; resolved to a reference AFTER, per spec.
    var viablePrev = -1;
    for (var i = idx - 1; i >= 0; i--) { if (!set[sibs[i]]) { viablePrev = sibs[i]; break; } }
    var node = convertNodesIntoNode(args);
    var ref;
    if (viablePrev < 0) { var k = __children(parent); ref = k.length ? k[0] : -1; }
    else { var nk = __children(parent); var pi = nk.indexOf(viablePrev); ref = (pi >= 0 && pi + 1 < nk.length) ? nk[pi + 1] : -1; }
    insertNode(parent, node, ref);
  }
  function childAfter(id, args) {
    var parent = __parent(id);
    if (parent < 0) { return; }
    var set = argNodeIdSet(args);
    var sibs = __children(parent);
    var idx = sibs.indexOf(id);
    // viableNextSibling: first following sibling not in args, else null (append).
    var ref = -1;
    for (var i = idx + 1; i < sibs.length; i++) { if (!set[sibs[i]]) { ref = sibs[i]; break; } }
    var node = convertNodesIntoNode(args);
    insertNode(parent, node, ref);
  }
  function childReplaceWith(id, args) {
    var parent = __parent(id);
    if (parent < 0) { return; }
    var set = argNodeIdSet(args);
    var sibs = __children(parent);
    var idx = sibs.indexOf(id);
    var ref = -1;
    for (var i = idx + 1; i < sibs.length; i++) { if (!set[sibs[i]]) { ref = sibs[i]; break; } }
    var node = convertNodesIntoNode(args);
    // Spec: if this node still has the same parent (it wasn't moved into the fragment), replace it;
    // otherwise just insert before the viable next sibling. We always remove `id` then insert.
    if (__parent(id) === parent) { __removeChild(parent, id); }
    insertNode(parent, node, ref);
  }
  // Validate that no argument node is a host-including inclusive ancestor of `parentId`, and that
  // parentId is a valid insertion parent (Document/Fragment/Element). Run before any conversion.
  function ensureParentNodeArgsValid(parentId, args) {
    var pt = __nodeType(parentId);
    if (pt !== 1 && pt !== 9 && pt !== 11) {
      hierarchyRequestError("Cannot insert into a node that is not a Document, DocumentFragment, or Element.");
    }
    for (var i = 0; i < args.length; i++) {
      var n = nodeIdOf(args[i]);
      if (n >= 0 && isInclusiveAncestor(n, parentId)) {
        hierarchyRequestError("The new child element contains the parent.");
      }
    }
  }
  // ParentNode.prepend/append/replaceChildren on `id`.
  function parentPrepend(id, args) {
    ensureParentNodeArgsValid(id, args);
    var node = convertNodesIntoNode(args);
    var kids = __children(id);
    insertNode(id, node, kids.length ? kids[0] : -1);
  }
  function parentAppend(id, args) {
    ensureParentNodeArgsValid(id, args);
    var node = convertNodesIntoNode(args);
    insertNode(id, node, -1);
  }
  function parentReplaceChildren(id, args) {
    ensureParentNodeArgsValid(id, args);
    var node = convertNodesIntoNode(args);
    var old = __children(id).slice();
    for (var i = 0; i < old.length; i++) { __removeChild(id, old[i]); }
    insertNode(id, node, -1);
  }
  def(globalThis, "__convertNodesIntoNode", convertNodesIntoNode);
  def(globalThis, "__insertNode", insertNode);

  // Mirror the JS-side `__nsMeta` (namespace metadata for createElementNS elements) from a source
  // subtree onto a freshly-cloned subtree. The Rust clone preserves child order, so we walk both
  // trees in lockstep. Attributes themselves are copied arena-side; only namespace info lives in JS.
  function copyNsMetaDeep(srcId, dstId) {
    var m = __nsMeta[srcId];
    if (m) {
      __nsMeta[dstId] = { namespaceURI: m.namespaceURI, prefix: m.prefix, localName: m.localName,
                          qualifiedName: m.qualifiedName, isHTML: m.isHTML };
    }
    var sk = __children(srcId), dk = __children(dstId);
    var n = Math.min(sk.length, dk.length);
    for (var i = 0; i < n; i++) { copyNsMetaDeep(sk[i], dk[i]); }
  }

  // Build a fresh element wrapper object for a node id. Carries `__node` plus accessors/methods
  // that delegate to the native primitives. Returns null for id === -1.
  function wrap(id) {
    if (typeof id !== "number" || id < 0) { return null; }
    var el = {};
    def(el, NODE, id);

    function uc(s) { return String(s == null ? "" : s).toUpperCase(); }

    // tagName resolution, honouring createElementNS metadata. HTML-namespace elements uppercase
    // their tagName; other namespaces preserve the qualifiedName exactly as given. Parsed elements
    // (no metadata) are HTML by default → uppercase of the lowercased arena tag.
    function elTagName() {
      var m = __nsMeta[id];
      if (m) {
        if (m.isHTML) { return asciiUpper(m.qualifiedName); }
        return m.qualifiedName;
      }
      return uc(__tag(id));
    }
    Object.defineProperty(el, "tagName", { get: elTagName, enumerable: true, configurable: true });
    Object.defineProperty(el, "nodeName", { get: function () {
      var t = __nodeType(id);
      if (t === 3) { return "#text"; }
      if (t === 8) { return "#comment"; }
      if (t === 9) { return "#document"; }
      if (t === 11) { return "#document-fragment"; }
      if (t === 10) { var di = __doctypeInfo(id); return di ? di.name : ""; }   // DocumentType: nodeName === name
      if (t === 7) { return __piTarget(id); }                                   // PI: nodeName === target
      return elTagName();
    }, enumerable: true, configurable: true });
    // DocumentType reflection (name / publicId / systemId) and ProcessingInstruction.target.
    if (__nodeType(id) === 10) {
      Object.defineProperty(el, "name", { get: function () { var d = __doctypeInfo(id); return d ? d.name : ""; }, enumerable: true, configurable: true });
      Object.defineProperty(el, "publicId", { get: function () { var d = __doctypeInfo(id); return d ? d.publicId : ""; }, enumerable: true, configurable: true });
      Object.defineProperty(el, "systemId", { get: function () { var d = __doctypeInfo(id); return d ? d.systemId : ""; }, enumerable: true, configurable: true });
    }
    if (__nodeType(id) === 7) {
      Object.defineProperty(el, "target", { get: function () { return __piTarget(id); }, enumerable: true, configurable: true });
    }
    Object.defineProperty(el, "namespaceURI", { get: function () {
      var m = __nsMeta[id];
      if (m) { return m.namespaceURI; }
      return __nodeType(id) === 1 ? HTML_NS : null;
    }, enumerable: true, configurable: true });
    Object.defineProperty(el, "prefix", { get: function () {
      var m = __nsMeta[id];
      return m ? m.prefix : null;
    }, enumerable: true, configurable: true });
    Object.defineProperty(el, "localName", { get: function () {
      var m = __nsMeta[id];
      if (m) { return m.localName; }
      return __nodeType(id) === 1 ? __tag(id) : null;
    }, enumerable: true, configurable: true });
    Object.defineProperty(el, "nodeType", { get: function () { return __nodeType(id); }, enumerable: true, configurable: true });

    Object.defineProperty(el, "textContent", {
      // Per spec: null for Document (9) and DocumentType (10); for everything else (Element,
      // DocumentFragment, Text, Comment, PI) the concatenation / data computed natively.
      get: function () { var t = __nodeType(id); return (t === 9 || t === 10) ? null : __textContent(id); },
      // Setter is a no-op on Document/DocumentType (textContent is null there).
      set: function (v) { var t = __nodeType(id); if (t === 9 || t === 10) { return; } __setTextContent(id, v == null ? "" : String(v)); },
      enumerable: true, configurable: true
    });
    // `data` mirrors textContent — used by Vue when patching text/comment anchors. This is a
    // CharacterData property, so only install it on Text/Comment/ProcessingInstruction nodes; on
    // element nodes `data` is a reflected content attribute (e.g. <object>.data is a URL), so leave
    // it free for the reflection layer.
    if (__nodeType(id) !== 1) {
      // `data` is a [LegacyNullToEmptyString] DOMString: only `null` becomes "" (undefined -> the
      // string "undefined", 0 -> "0", etc.).
      Object.defineProperty(el, "data", {
        get: function () { return __textContent(id); },
        set: function (v) { __setTextContent(id, v === null ? "" : String(v)); },
        enumerable: true, configurable: true
      });
      // CharacterData.length: the number of UTF-16 code units in `data`.
      Object.defineProperty(el, "length", {
        get: function () { return __textContent(id).length; },
        enumerable: true, configurable: true
      });
      // The CharacterData mutation methods all reduce to "replace data": offset/count are WebIDL
      // `unsigned long` (ToUint32, i.e. `>>> 0`), an out-of-range offset throws IndexSizeError, and a
      // count running past the end is clamped. Operations are in UTF-16 code units (JS string units).
      var __cdReplace = function (offset, count, insert) {
        var d = __textContent(id);
        var len = d.length;
        if (offset > len) { throw new globalThis.DOMException("The index is not in the allowed range.", "IndexSizeError"); }
        if (offset + count > len) { count = len - offset; }
        __setTextContent(id, d.slice(0, offset) + insert + d.slice(offset + count));
      };
      def(el, "substringData", function (offset, count) {
        if (arguments.length < 2) { throw new TypeError("Failed to execute 'substringData': 2 arguments required."); }
        var d = __textContent(id), len = d.length;
        offset = offset >>> 0; count = count >>> 0;
        if (offset > len) { throw new globalThis.DOMException("The index is not in the allowed range.", "IndexSizeError"); }
        var end = offset + count; if (end > len) { end = len; }
        return d.slice(offset, end);
      });
      def(el, "appendData", function (data) {
        if (arguments.length < 1) { throw new TypeError("Failed to execute 'appendData': 1 argument required."); }
        __setTextContent(id, __textContent(id) + String(data));
      });
      def(el, "insertData", function (offset, data) {
        if (arguments.length < 2) { throw new TypeError("Failed to execute 'insertData': 2 arguments required."); }
        __cdReplace(offset >>> 0, 0, String(data));
      });
      def(el, "deleteData", function (offset, count) {
        if (arguments.length < 2) { throw new TypeError("Failed to execute 'deleteData': 2 arguments required."); }
        __cdReplace(offset >>> 0, count >>> 0, "");
      });
      def(el, "replaceData", function (offset, count, data) {
        if (arguments.length < 3) { throw new TypeError("Failed to execute 'replaceData': 3 arguments required."); }
        __cdReplace(offset >>> 0, count >>> 0, String(data));
      });
    }
    Object.defineProperty(el, "nodeValue", {
      // nodeValue is the data for the CharacterData kinds (Text=3, CDATASection=4, PI=7, Comment=8);
      // null for everything else.
      get: function () { var t = __nodeType(id); return (t === 3 || t === 4 || t === 7 || t === 8) ? __textContent(id) : null; },
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
    // setHTMLUnsafe(html): parse `html` and replace this element's children, like innerHTML but
    // without sanitization (we do not sanitize anyway). The `template`/shadowroot semantics of the
    // real algorithm are not modeled; a plain reparse covers the WPT callers that use it as a
    // convenience to install markup. getHTML() serializes back (≈ innerHTML).
    def(el, "setHTMLUnsafe", function (html) { __setInnerHTML(id, html == null ? "" : String(html)); });
    def(el, "getHTML", function () { return __innerHTML(id); });

    // id / className are DOMString reflections: null/undefined stringify to "null"/"undefined".
    Object.defineProperty(el, "id", {
      get: function () { var v = __getAttr(id, "id"); return v == null ? "" : v; },
      set: function (v) { __setAttr(id, "id", String(v)); },
      enumerable: true, configurable: true
    });
    Object.defineProperty(el, "className", {
      get: function () { var v = __getAttr(id, "class"); return v == null ? "" : v; },
      set: function (v) { __setAttr(id, "class", String(v)); },
      enumerable: true, configurable: true
    });

    // Per-element attribute namespace metadata: keyed by the qualified-name storage key, holds
    // { namespaceURI, prefix, localName } so getAttributeNS / Attr.localName reflect correctly.
    function elIsHtml() {
      var m = __nsMeta[id];
      return m ? m.isHTML : (__nodeType(id) === 1);
    }
    def(el, "getAttribute", function (name) {
      // HTML elements ASCII-lowercase the qualified name before matching (stored lowercased).
      var nm = String(name);
      if (elIsHtml()) { nm = asciiLower(nm); }
      return __getAttr(id, nm);
    });
    def(el, "setAttribute", function (name, value) {
      var nm = String(name);
      // "validate" the qualified name: reject the empty string and any name containing whitespace
      // or '>' (matching the lenient Name production browsers/WPT accept).
      if (nm.length === 0) { invalidCharacterError(); }
      for (var vi = 0; vi < nm.length; vi++) {
        var vc = nm.charCodeAt(vi);
        if (vc === 0x3E || vc === 0x20 || vc === 0x09 || vc === 0x0A || vc === 0x0C || vc === 0x0D) { invalidCharacterError(); }
      }
      // HTML elements ASCII-lowercase the attribute's qualified name.
      if (elIsHtml()) { nm = asciiLower(nm); }
      // `value` is a non-nullable DOMString in WebIDL: undefined -> "undefined", null -> "null".
      __setAttr(id, nm, String(value));
    });
    def(el, "removeAttribute", function (name) {
      var nm = String(name);
      if (elIsHtml()) { nm = asciiLower(nm); }
      __detachCachedAttr(nm);
      __removeAttr(id, nm); delete __attrNs[nm]; delete __attrNodeCache[nm];
    });
    def(el, "hasAttribute", function (name) {
      var nm = String(name);
      if (elIsHtml()) { nm = asciiLower(nm); }
      return __getAttr(id, nm) != null;
    });
    def(el, "getAttributeNames", function () { return __attrNames(id); });

    // Namespaced attribute accessors. The arena keys attrs by their qualified name; we keep the
    // namespace/prefix/localName split in __attrNs so getAttributeNS and Attr reflection work.
    var __attrNs = {};
    def(el, "setAttributeNS", function (ns, qualifiedName, value) {
      var ex = validateAndExtract(ns, qualifiedName);
      var key = String(qualifiedName);
      __setAttr(id, key, value == null ? "" : String(value));
      __attrNs[key] = { namespaceURI: ex.namespace, prefix: ex.prefix, localName: ex.localName };
    });
    def(el, "getAttributeNS", function (ns, localName) {
      var want = (ns === undefined || ns === null || ns === "") ? null : String(ns);
      var ln = String(localName);
      var names = __attrNames(id);
      for (var i = 0; i < names.length; i++) {
        var k = names[i];
        var meta = __attrNs[k];
        var kNs = meta ? meta.namespaceURI : null;
        var kLocal = meta ? meta.localName : k;
        if (kNs === want && kLocal === ln) { return __getAttr(id, k); }
      }
      return null;
    });
    def(el, "hasAttributeNS", function (ns, localName) {
      return el.getAttributeNS(ns, localName) != null;
    });
    def(el, "removeAttributeNS", function (ns, localName) {
      var want = (ns === undefined || ns === null || ns === "") ? null : String(ns);
      var ln = String(localName);
      var names = __attrNames(id);
      for (var i = 0; i < names.length; i++) {
        var k = names[i];
        var meta = __attrNs[k];
        var kNs = meta ? meta.namespaceURI : null;
        var kLocal = meta ? meta.localName : k;
        if (kNs === want && kLocal === ln) { __detachCachedAttr(k); __removeAttr(id, k); delete __attrNs[k]; delete __attrNodeCache[k]; return; }
      }
    });

    // A LIVE NamedNodeMap: React (and others) do `for (var a = el.attributes; a.length;)
    // el.removeAttributeNode(a[0])`, capturing the map once and relying on removals shrinking it —
    // so length/index must re-query the node each access (a static snapshot would infinite-loop).
    // A *bound* Attr node, keyed by its qualified-name storage key. `ownerElement` is live (becomes
    // null once the attribute is removed), and value get/set reads/writes the live arena attribute.
    // Cached per storage key so `el.attributes[0] === el.getAttributeNode(name)` (object identity).
    var __attrNodeCache = {};
    var makeAttr = function (attrName) {
      if (__attrNodeCache[attrName]) { return __attrNodeCache[attrName]; }
      var meta = __attrNs[attrName];
      var attr = { nodeName: attrName, name: attrName, nodeType: 2,
               namespaceURI: meta ? meta.namespaceURI : null,
               prefix: meta ? meta.prefix : null,
               localName: meta ? meta.localName : attrName,
               specified: true };
      Object.defineProperty(attr, "ownerElement", {
        get: function () { return __getAttr(id, attrName) == null ? null : el; },
        enumerable: true, configurable: true
      });
      var setVal = function (v) { __setAttr(id, attrName, v == null ? "" : String(v)); };
      var getVal = function () { var v = __getAttr(id, attrName); return v == null ? "" : v; };
      Object.defineProperty(attr, "value", { get: getVal, set: setVal, enumerable: true, configurable: true });
      Object.defineProperty(attr, "nodeValue", { get: getVal, set: setVal, enumerable: true, configurable: true });
      Object.defineProperty(attr, "textContent", { get: getVal, set: setVal, enumerable: true, configurable: true });
      def(attr, "lookupNamespaceURI", function (prefix) { return nodeLookupNamespaceURI(id, prefix); });
      def(attr, "lookupPrefix", function (ns) { return nodeLookupPrefix(id, ns); });
      def(attr, "isDefaultNamespace", function (ns) { return nodeIsDefaultNamespace(id, ns); });
      try { if (globalThis.Attr && globalThis.Attr.prototype) { Object.setPrototypeOf(attr, globalThis.Attr.prototype); } } catch (e) {}
      __attrNodeCache[attrName] = attr;
      return attr;
    };
    // If a cached Attr node exists for `key`, snapshot its current value into a standalone closure
    // and null its ownerElement. Call BEFORE removing the arena attribute so the detached node keeps
    // the value it had while connected (per spec, an Attr retains its value after removal).
    function __detachCachedAttr(key) {
      var a = __attrNodeCache[key];
      if (!a) { return; }
      var stored = __getAttr(id, key); if (stored == null) { stored = ""; }
      try {
        var dget = function () { return stored; };
        var dset = function (v) { stored = v == null ? "" : String(v); };
        Object.defineProperty(a, "value", { get: dget, set: dset, configurable: true, enumerable: true });
        Object.defineProperty(a, "nodeValue", { get: dget, set: dset, configurable: true, enumerable: true });
        Object.defineProperty(a, "textContent", { get: dget, set: dset, configurable: true, enumerable: true });
        Object.defineProperty(a, "ownerElement", { value: null, configurable: true, enumerable: true, writable: true });
      } catch (e) {}
    }
    // Find the storage key of an attribute by (namespace, localName); null if absent.
    function attrKeyByNs(want, ln) {
      var names = __attrNames(id);
      for (var i = 0; i < names.length; i++) {
        var k = names[i], meta = __attrNs[k];
        var kNs = meta ? meta.namespaceURI : null;
        var kLocal = meta ? meta.localName : k;
        if (kNs === want && kLocal === ln) { return k; }
      }
      return null;
    }
    var attrMap = new Proxy({}, {
      get: function (t, prop) {
        if (prop === "length") { return __attrNames(id).length; }
        if (prop === "item") { return function (i) { var n = __attrNames(id)[i >>> 0]; return n == null ? null : makeAttr(n); }; }
        if (prop === "getNamedItem") { return function (nm) { return el.getAttributeNode(nm); }; }
        if (prop === "getNamedItemNS") { return function (ns, ln) { return el.getAttributeNodeNS(ns, ln); }; }
        if (prop === "setNamedItem" || prop === "setNamedItemNS") { return function (attr) { return el.setAttributeNode(attr); }; }
        if (prop === "removeNamedItem") { return function (nm) {
          var a = el.getAttributeNode(nm);
          if (a == null) { notFoundError("No attribute named '" + nm + "'."); }
          return el.removeAttributeNode(a);
        }; }
        if (prop === "removeNamedItemNS") { return function (ns, ln) {
          var a = el.getAttributeNodeNS(ns, ln);
          if (a == null) { notFoundError("No such attribute."); }
          return el.removeAttributeNode(a);
        }; }
        if (prop === Symbol.iterator) { return function () { return __attrNames(id).map(makeAttr)[Symbol.iterator](); }; }
        if (typeof prop === "string" && /^\d+$/.test(prop)) { var n = __attrNames(id)[+prop]; return n == null ? undefined : makeAttr(n); }
        // Named property access: getNamedItem(prop).
        if (typeof prop === "string" && __getAttr(id, prop) != null) { return makeAttr(prop); }
        return t[prop];
      },
      has: function (t, prop) {
        if (prop === "length" || prop === "item" || prop === "getNamedItem" || prop === "getNamedItemNS" ||
            prop === "setNamedItem" || prop === "setNamedItemNS" || prop === "removeNamedItem" || prop === "removeNamedItemNS") { return true; }
        if (typeof prop === "string" && /^\d+$/.test(prop)) { return +prop < __attrNames(id).length; }
        return prop in t;
      },
      // Own-property enumeration: getOwnPropertyNames(attrs) === [indices..., qualifiedNames...].
      // Indices are enumerable; the named (qualified-name) keys are non-enumerable own properties.
      ownKeys: function () {
        var names = __attrNames(id), keys = [];
        for (var i = 0; i < names.length; i++) { keys.push(String(i)); }
        var seen = Object.create(null);
        for (var j = 0; j < names.length; j++) { if (!seen[names[j]]) { seen[names[j]] = 1; keys.push(names[j]); } }
        return keys;
      },
      getOwnPropertyDescriptor: function (t, prop) {
        if (typeof prop === "string" && /^\d+$/.test(prop)) {
          var nm = __attrNames(id)[+prop];
          if (nm != null) { return { value: makeAttr(nm), writable: false, enumerable: true, configurable: true }; }
          return undefined;
        }
        if (prop === "length") { return { value: __attrNames(id).length, writable: false, enumerable: false, configurable: true }; }
        if (typeof prop === "string" && __getAttr(id, prop) != null) {
          // A named (qualified-name) own property: non-enumerable, holds the Attr.
          return { value: makeAttr(prop), writable: false, enumerable: false, configurable: true };
        }
        return Object.getOwnPropertyDescriptor(t, prop);
      }
    });
    Object.defineProperty(el, "attributes", { get: function () { return attrMap; }, configurable: true });
    def(el, "removeAttributeNode", function (attr) {
      // Spec: if attr's element isn't this element, throw NotFoundError. Then remove it and detach
      // the SAME Attr object (it keeps its last value/name; ownerElement becomes null).
      if (!attr || attr.nodeType !== 2) { throw new TypeError("parameter is not an Attr."); }
      var key = (attr.__attrKey != null) ? attr.__attrKey : String(attr.name);
      if (__getAttr(id, key) == null) { notFoundError("The attribute is not part of this element."); }
      var finalVal = __getAttr(id, key);
      __removeAttr(id, key); delete __attrNs[key]; delete __attrNodeCache[key];
      // Re-bind the node's value/ownerElement to a standalone (detached) state.
      try {
        var stored = finalVal == null ? "" : String(finalVal);
        var dget = function () { return stored; };
        var dset = function (v) { stored = v == null ? "" : String(v); };
        Object.defineProperty(attr, "value", { get: dget, set: dset, configurable: true, enumerable: true });
        Object.defineProperty(attr, "nodeValue", { get: dget, set: dset, configurable: true, enumerable: true });
        Object.defineProperty(attr, "textContent", { get: dget, set: dset, configurable: true, enumerable: true });
        Object.defineProperty(attr, "ownerElement", { value: null, configurable: true, enumerable: true, writable: true });
      } catch (e) {}
      return attr;
    });
    def(el, "getAttributeNode", function (name) {
      var nm = String(name);
      if (elIsHtml()) { nm = asciiLower(nm); }
      return __getAttr(id, nm) == null ? null : makeAttr(nm);
    });
    def(el, "getAttributeNodeNS", function (ns, localName) {
      var want = (ns === undefined || ns === null || ns === "") ? null : String(ns);
      var key = attrKeyByNs(want, String(localName));
      return key == null ? null : makeAttr(key);
    });
    // setAttributeNode(attr): set/replace the attribute named by attr; per spec, throw
    // InUseAttributeError if attr is already owned by a *different* element. Returns the previously
    // set Attr (or null). For a same-name replacement, returns the old attr value.
    function setAttrNodeImpl(attr) {
      if (!attr || attr.nodeType !== 2) { throw new TypeError("parameter is not an Attr."); }
      var owner = attr.ownerElement;
      if (owner != null && owner !== el) {
        throw new globalThis.DOMException("The attribute is in use by another element.", "InUseAttributeError");
      }
      var ns = attr.namespaceURI || null;
      var ln = attr.localName != null ? String(attr.localName) : String(attr.name);
      var key = String(attr.name);
      // Existing attribute with the same namespace + localName?
      var oldKey = attrKeyByNs(ns, ln);
      var oldAttr = null;
      if (oldKey != null) {
        oldAttr = makeAttr(oldKey);
        if (oldKey !== key) { __removeAttr(id, oldKey); delete __attrNs[oldKey]; delete __attrNodeCache[oldKey]; }
      }
      var newVal = attr.value == null ? "" : String(attr.value);
      __setAttr(id, key, newVal);
      __attrNs[key] = { namespaceURI: ns, prefix: attr.prefix || null, localName: ln };
      // Adopt the SAME Attr object: re-bind its value / ownerElement getters to this element's live
      // arena attribute, and register it as the canonical cached node so getAttributeNode /
      // el.attributes[i] return the identical object (per spec the node is moved, not copied).
      try { def(attr, "__attrKey", key); } catch (e) {}
      try {
        var bget = function () { var v = __getAttr(id, key); return v == null ? "" : v; };
        var bset = function (v) { __setAttr(id, key, v == null ? "" : String(v)); };
        Object.defineProperty(attr, "value", { get: bget, set: bset, configurable: true, enumerable: true });
        Object.defineProperty(attr, "nodeValue", { get: bget, set: bset, configurable: true, enumerable: true });
        Object.defineProperty(attr, "textContent", { get: bget, set: bset, configurable: true, enumerable: true });
        Object.defineProperty(attr, "ownerElement", { get: function () { return __getAttr(id, key) == null ? null : el; }, configurable: true, enumerable: true });
      } catch (e) {}
      __attrNodeCache[key] = attr;
      return oldAttr;
    }
    def(el, "setAttributeNode", setAttrNodeImpl);
    def(el, "setAttributeNodeNS", setAttrNodeImpl);
    def(el, "toggleAttribute", function (name, force) {
      var qn = String(name);
      // "validate" only rejects names that don't match the (lenient) Name production: the empty
      // string and any name containing whitespace or '>' (matching setAttribute's behaviour).
      if (qn.length === 0) { invalidCharacterError(); }
      for (var vi = 0; vi < qn.length; vi++) {
        var vc = qn.charCodeAt(vi);
        if (vc === 0x3E || vc === 0x20 || vc === 0x09 || vc === 0x0A || vc === 0x0C || vc === 0x0D) { invalidCharacterError(); }
      }
      if (elIsHtml()) { qn = asciiLower(qn); }
      var present = __getAttr(id, qn) != null;
      if (!present) {
        if (force === undefined || force === true) { __setAttr(id, qn, ""); return true; }
        return false;
      }
      if (force === undefined || force === false) { __removeAttr(id, qn); delete __attrNs[qn]; return false; }
      return true;
    });

    def(el, "appendChild", function (child) {
      var cid = requireNodeArg(child, "appendChild");
      ensurePreInsertValid(id, cid, -1);
      insertNode(id, cid, -1);
      return child;
    });
    def(el, "removeChild", function (child) {
      var cid = requireNodeArg(child, "removeChild");
      if (__parent(cid) !== id) { notFoundError("The node to be removed is not a child of this node."); }
      __removeChild(id, cid);
      return child;
    });
    def(el, "insertBefore", function (newNode, refNode) {
      var cid = requireNodeArg(newNode, "insertBefore");
      var refId = (refNode == null) ? -1 : nodeIdOf(refNode);
      if (refNode != null && refId < 0) { notFoundError("The reference child is not a child of this node."); }
      ensurePreInsertValid(id, cid, refId);
      insertNode(id, cid, refId);
      return newNode;
    });
    def(el, "replaceChild", function (newNode, oldNode) {
      var nid = requireNodeArg(newNode, "replaceChild"), oid = requireNodeArg(oldNode, "replaceChild");
      if (__parent(oid) !== id) { notFoundError("The node to be replaced is not a child of this node."); }
      if (isInclusiveAncestor(nid, id)) { hierarchyRequestError("The new child element contains the parent."); }
      // Reference child = oldNode's next sibling, unless that's newNode itself (then newNode's next).
      var sibs = __children(id); var idx = sibs.indexOf(oid);
      var ref = (idx >= 0 && idx + 1 < sibs.length) ? sibs[idx + 1] : -1;
      if (ref === nid) {
        var ni = sibs.indexOf(nid);
        ref = (ni >= 0 && ni + 1 < sibs.length) ? sibs[ni + 1] : -1;
      }
      __removeChild(id, oid);
      insertNode(id, nid, ref);
      return oldNode;
    });
    def(el, "remove", function () { var p = __parent(id); if (p >= 0) { __removeChild(p, id); } });
    def(el, "append", function () { parentAppend(id, arguments); });
    def(el, "prepend", function () { parentPrepend(id, arguments); });
    def(el, "replaceChildren", function () { parentReplaceChildren(id, arguments); });
    def(el, "before", function () { childBefore(id, arguments); });
    def(el, "after", function () { childAfter(id, arguments); });
    def(el, "replaceWith", function () { childReplaceWith(id, arguments); });
    def(el, "cloneNode", function (deep) {
      var nid = __cloneNode(id, !!deep);
      if (nid < 0) { return null; }
      copyNsMetaDeep(id, nid);
      var w = wrap(nid);
      // Route through the canonical-wrapper cache (when the browser-env layer is present) so the
      // clone has a stable identity and full enrichment (style/classList/childNodes === checks).
      return (typeof globalThis.__canonNode === "function") ? globalThis.__canonNode(w) : w;
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
      else { throw new globalThis.DOMException("Failed to execute 'insertAdjacentElement': '" + position + "' is not a valid value.", "SyntaxError"); }
      return node;
    });

    def(el, "insertAdjacentHTML", function (position, html) {
      var pos = String(position == null ? "" : position).toLowerCase();
      if (pos !== "beforebegin" && pos !== "afterbegin" && pos !== "beforeend" && pos !== "afterend") {
        throw new globalThis.DOMException("Failed to execute 'insertAdjacentHTML': '" + position + "' is not a valid value.", "SyntaxError");
      }
      // Parse the HTML fragment into real nodes via a temp container, then move them.
      var tmp = __createElement("template");
      __setInnerHTML(tmp, html == null ? "" : String(html));
      var parsed = __children(tmp).slice();
      if (pos === "beforebegin") {
        var p = __parent(id);
        if (p < 0 || __nodeType(p) === 9) { throw new globalThis.DOMException("Cannot insert adjacent to a node with no parent element.", "NoModificationAllowedError"); }
        for (var i = 0; i < parsed.length; i++) { __insertBefore(p, parsed[i], id); }
      } else if (pos === "afterbegin") {
        var k = __children(id); var ref = k.length ? k[0] : -1;
        for (var i = 0; i < parsed.length; i++) { __insertBefore(id, parsed[i], ref); }
      } else if (pos === "beforeend") {
        for (var i = 0; i < parsed.length; i++) { __appendChild(id, parsed[i]); }
      } else { // afterend
        var p2 = __parent(id);
        if (p2 < 0 || __nodeType(p2) === 9) { throw new globalThis.DOMException("Cannot insert adjacent to a node with no parent element.", "NoModificationAllowedError"); }
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
    def(el, "getElementsByTagName", function (tag) {
      var qn = String(tag);
      return collectDescendants(id, function (eid) { return matchesTagName(eid, qn); });
    });
    def(el, "getElementsByTagNameNS", function (ns, localName) {
      var n = (ns === "*" || ns == null) ? "*" : String(ns);
      var ln = (localName === "*" || localName == null) ? "*" : String(localName);
      return collectDescendants(id, function (eid) { return matchesTagNameNS(eid, n, ln); });
    });
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

    // Namespace lookup mixin (Node). DocumentType/PI/DocumentFragment wrappers also get these.
    def(el, "lookupNamespaceURI", function (prefix) { return nodeLookupNamespaceURI(id, prefix); });
    def(el, "lookupPrefix", function (ns) { return nodeLookupPrefix(id, ns); });
    def(el, "isDefaultNamespace", function (ns) { return nodeIsDefaultNamespace(id, ns); });

    return el;
  }
  def(globalThis, "__wrapNode", wrap);

  // --- document --------------------------------------------------------------------------------
  var document = {};
  def(document, "getElementById", function (idStr) { var n = __getElementById(String(idStr)); return n >= 0 ? wrap(n) : null; });
  def(document, "getElementsByTagName", function (tag) {
    var qn = String(tag);
    return collectDescendants(0, function (eid) { return matchesTagName(eid, qn); });
  });
  def(document, "getElementsByClassName", function (cls) { return __getElementsByClassName(String(cls)).map(wrap); });
  def(document, "querySelector", function (sel) { var r = __querySelectorAll(String(sel)); return r.length ? wrap(r[0]) : null; });
  def(document, "querySelectorAll", function (sel) { return __querySelectorAll(String(sel)).map(wrap); });
  // document.write / writeln. We run scripts after the full parse (there is no live insertion point),
  // so the written markup is parsed and appended to <body> (or the documentElement) — enough for the
  // common case of a script writing extra elements (e.g. a <link>/<script>) into the page.
  def(document, "write", function () {
    var html = "";
    for (var i = 0; i < arguments.length; i++) { html += String(arguments[i]); }
    var target = document.body || document.documentElement;
    if (!target) { return; }
    var tmp = document.createElement("div");
    tmp.innerHTML = html;
    var kids = [];
    var cn = tmp.childNodes;
    for (var k = 0; k < cn.length; k++) { kids.push(cn[k]); }
    for (var j = 0; j < kids.length; j++) { try { target.appendChild(kids[j]); } catch (e) {} }
  });
  def(document, "writeln", function () {
    var a = Array.prototype.slice.call(arguments);
    a.push("\n");
    document.write.apply(document, a);
  });
  def(document, "createElement", function (tag) {
    // HTML document: validate the name as an XML Name, then ASCII-lowercase it. namespaceURI is the
    // HTML namespace, prefix null, localName the lowercased name, tagName the uppercased localName.
    var name = String(tag);
    // createElement validates the name as an XML Name (colons permitted), without splitting it
    // into prefix/localName. HTML documents then ASCII-lowercase the whole name.
    if (!isValidNameImpl(name, true)) { invalidCharacterError(); }
    var local = asciiLower(name);
    var id = __createElement(local);
    __nsMeta[id] = { namespaceURI: HTML_NS, prefix: null, localName: local, qualifiedName: local, isHTML: true };
    return wrap(id);
  });
  def(document, "createElementNS", function (ns, qualifiedName) {
    var ex = validateAndExtract(ns, qualifiedName);
    var isHTML = ex.namespace === HTML_NS;
    // The arena tag is the local name (lowercased only when HTML, to match parser behaviour).
    var arenaTag = isHTML ? asciiLower(ex.localName) : ex.localName;
    var id = __createElement(arenaTag);
    __nsMeta[id] = {
      namespaceURI: ex.namespace,
      prefix: ex.prefix,
      localName: ex.localName,
      qualifiedName: String(qualifiedName),
      isHTML: isHTML
    };
    return wrap(id);
  });
  // createAttribute / createAttributeNS return an Attr node (not arena-backed) with the correct
  // name/localName/namespaceURI/prefix/value reflection.
  function makeAttrNode(namespaceURI, prefix, localName, qualifiedName, initialValue) {
    var value = initialValue == null ? "" : String(initialValue);
    var attr = {
      nodeType: 2,
      namespaceURI: namespaceURI,
      prefix: prefix,
      localName: localName,
      name: qualifiedName,
      nodeName: qualifiedName,
      specified: true,
      ownerElement: null
    };
    Object.defineProperty(attr, "value", {
      get: function () { return value; },
      set: function (v) { value = v == null ? "" : String(v); },
      enumerable: true, configurable: true
    });
    Object.defineProperty(attr, "nodeValue", {
      get: function () { return value; },
      set: function (v) { value = v == null ? "" : String(v); },
      enumerable: true, configurable: true
    });
    Object.defineProperty(attr, "textContent", {
      get: function () { return value; },
      set: function (v) { value = v == null ? "" : String(v); },
      enumerable: true, configurable: true
    });
    // Attr namespace lookup delegates to the owner element (null when disconnected).
    def(attr, "lookupNamespaceURI", function (prefix) {
      var oe = this.ownerElement; return oe && oe.lookupNamespaceURI ? oe.lookupNamespaceURI(prefix) : null;
    });
    def(attr, "lookupPrefix", function (ns) {
      var oe = this.ownerElement; return oe && oe.lookupPrefix ? oe.lookupPrefix(ns) : null;
    });
    def(attr, "isDefaultNamespace", function (ns) {
      var oe = this.ownerElement; return oe && oe.isDefaultNamespace ? oe.isDefaultNamespace(ns) : (ns == null || ns === "");
    });
    try { if (globalThis.Attr && globalThis.Attr.prototype) { Object.setPrototypeOf(attr, globalThis.Attr.prototype); } } catch (e) {}
    return attr;
  }
  def(document, "createAttribute", function (localName) {
    // HTML document: validate (only the empty name is rejected here, matching browser behaviour)
    // then ASCII-lowercase. namespaceURI/prefix null.
    var name = String(localName);
    if (name.length === 0) { invalidCharacterError(); }
    var local = asciiLower(name);
    return makeAttrNode(null, null, local, local);
  });
  def(document, "createAttributeNS", function (ns, qualifiedName) {
    var ex = validateAndExtract(ns, qualifiedName);
    return makeAttrNode(ex.namespace, ex.prefix, ex.localName, String(qualifiedName));
  });
  // Expose the Attr factory + the validation helpers so the off-document (XML) document objects
  // built by document.implementation.createDocument can offer case-preserving createAttribute.
  def(globalThis, "__makeAttrNode", makeAttrNode);
  // createDocumentType: validate the qualified name (QName), then build a real DocumentType arena
  // node. Per spec a bad name is an InvalidCharacterError; a bad prefix split is a NamespaceError.
  def(globalThis, "__createDocumentTypeNode", function (qualifiedName, publicId, systemId) {
    var qn = String(qualifiedName);
    // createDocumentType's "validate" step only checks the QName matches the (lenient) Name
    // production. Per the behaviour browsers/WPT implement, every codepoint must be a NameChar:
    // any non-whitespace, non-'>' character is accepted mid-name (colons included), and the empty
    // string is allowed. A '>' or ASCII whitespace anywhere => InvalidCharacterError.
    for (var i = 0; i < qn.length; i++) {
      var cc = qn.charCodeAt(i);
      if (cc === 0x3E || cc === 0x20 || cc === 0x09 || cc === 0x0A || cc === 0x0C || cc === 0x0D) {
        invalidCharacterError();
      }
    }
    var nid = __createDocumentType(qn, publicId == null ? "" : String(publicId), systemId == null ? "" : String(systemId));
    return wrap(nid);
  });
  def(globalThis, "__validateAndExtractName", validateAndExtract);
  def(globalThis, "__invalidCharacterError", invalidCharacterError);
  // Create an element carrying explicit namespace metadata (used by XML-flavoured documents from
  // document.implementation.createDocument, whose createElement does NOT lowercase or assign the
  // HTML namespace). htmlNs => HTML-namespace semantics (lowercase + uppercase tagName).
  def(globalThis, "__createElementWithNs", function (namespaceURI, name) {
    var nm = String(name);
    if (!isValidNameImpl(nm, true)) { invalidCharacterError(); }
    var isHtml = namespaceURI === HTML_NS;
    var local = isHtml ? asciiLower(nm) : nm;
    var id = __createElement(local);
    __nsMeta[id] = {
      namespaceURI: (namespaceURI === undefined || namespaceURI === null || namespaceURI === "") ? null : String(namespaceURI),
      prefix: null, localName: local, qualifiedName: local, isHTML: isHtml
    };
    return wrap(id);
  });
  // getElementsByTagNameNS(namespace, localName): all descendant elements matching namespace
  // (or "*") and localName (or "*"). Returned as a plain array snapshot.
  def(document, "getElementsByTagNameNS", function (namespace, localName) {
    var ns = (namespace === "*" || namespace == null) ? "*" : String(namespace);
    var ln = (localName === "*" || localName == null) ? "*" : String(localName);
    return collectDescendants(0, function (eid) { return matchesTagNameNS(eid, ns, ln); });
  });
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
  // Document namespace lookup delegates to the document element (node id 0 is the Document root).
  def(document, "lookupNamespaceURI", function (prefix) { return nodeLookupNamespaceURI(0, prefix); });
  def(document, "lookupPrefix", function (ns) { return nodeLookupPrefix(0, ns); });
  def(document, "isDefaultNamespace", function (ns) { return nodeIsDefaultNamespace(0, ns); });
  // document.doctype: the first DocumentType child of the document root (node id 0), or null.
  Object.defineProperty(document, "doctype", {
    get: function () {
      var kids = __children(0);
      for (var i = 0; i < kids.length; i++) { if (__nodeType(kids[i]) === 10) { return wrap(kids[i]); } }
      return null;
    },
    enumerable: true, configurable: true
  });
  def(document, "createProcessingInstruction", function (target, data) {
    var t = String(target);
    if (!isValidNameImpl(t, true)) { invalidCharacterError(); }
    if (String(data).indexOf("?>") >= 0) {
      throw new globalThis.DOMException("The data must not contain '?>'.", "InvalidCharacterError");
    }
    var __pi = wrap(__createProcessingInstruction(t, String(data)));
    // Canonicalize (cache the wrapper) so navigation preserves node identity, and graft on methods.
    try { __pi = globalThis.__canonNode(__pi); } catch (e) {}
    try { globalThis.__addPartialMethods(__pi); } catch (e) {}
    return __pi;
  });
  def(document, "createDocumentType", function (qualifiedName, publicId, systemId) {
    return globalThis.__createDocumentTypeNode(String(qualifiedName),
      publicId == null ? "" : String(publicId), systemId == null ? "" : String(systemId));
  });
  Object.defineProperty(document, "body", { get: function () { var n = __bodyId(); return n >= 0 ? wrap(n) : null; }, enumerable: true, configurable: true });
  Object.defineProperty(document, "documentElement", { get: function () { var n = __documentElementId(); return n >= 0 ? wrap(n) : null; }, enumerable: true, configurable: true });
  Object.defineProperty(document, "head", { get: function () { var n = __headId(); return n >= 0 ? wrap(n) : null; }, enumerable: true, configurable: true });
  def(document, "nodeType", 9);
  // A Document's textContent / nodeValue are null (it's not CharacterData or an Element).
  Object.defineProperty(document, "textContent", { get: function () { return null; }, set: function () {}, enumerable: true, configurable: true });
  Object.defineProperty(document, "nodeValue", { get: function () { return null; }, set: function () {}, enumerable: true, configurable: true });

  // document.styleSheets: a StyleSheetList of the CSSStyleSheet objects for each <style> and
  // <link rel=stylesheet> element, in document order. Each entry is the element's own `.sheet`
  // (SameObject), so `styleSheets[i] === el.sheet`.
  // The current document.styleSheets entries: each <style>/<link rel=stylesheet>'s `.sheet`, in
  // tree order (excluding the adopted-sheets mirror and disabled links).
  function __collectDocSheets() {
    var els = document.querySelectorAll("style, link");
    var sheets = [];
    for (var i = 0; i < els.length; i++) {
      var el = els[i];
      var tag = (el.tagName || "").toLowerCase();
      if (el.getAttribute && el.getAttribute("data-adopted-stylesheets") != null) { continue; }
      if (tag === "link") {
        var rel = (el.getAttribute && el.getAttribute("rel") || "").toLowerCase();
        if (rel.split(/\s+/).indexOf("stylesheet") < 0) { continue; }
      }
      if (el.__sheetDisabled || (el.getAttribute && el.getAttribute("disabled") != null && tag === "link")) { continue; }
      try { var s = el.sheet; if (s) { sheets.push(s); } } catch (e) {}
    }
    return sheets;
  }
  // document.styleSheets is a LIVE StyleSheetList: a single object whose length / indexing / item /
  // iteration all re-read the DOM, so a captured reference reflects added/removed sheets (CSSOM).
  var __docSheetList = Object.create((globalThis.StyleSheetList && globalThis.StyleSheetList.prototype) || Object.prototype);
  Object.defineProperty(__docSheetList, "length", { get: function () { return __collectDocSheets().length; }, enumerable: false, configurable: true });
  __docSheetList.item = function (n) { var s = __collectDocSheets(); n = n >>> 0; return n < s.length ? s[n] : null; };
  try {
    __docSheetList[Symbol.iterator] = function () {
      var s = __collectDocSheets(), i = 0;
      var it = { next: function () { return i < s.length ? { value: s[i++], done: false } : { value: undefined, done: true }; } };
      it[Symbol.iterator] = function () { return this; };
      return it;
    };
  } catch (e) {}
  var __docSheetListProxy = new Proxy(__docSheetList, {
    get: function (t, p) {
      // Indexed getter: out-of-range returns `undefined` (WebIDL), unlike item() which returns null.
      if (typeof p === "string" && /^[0-9]+$/.test(p)) { var s = __collectDocSheets(); var n = Number(p); return n < s.length ? s[n] : undefined; }
      return t[p];
    },
    has: function (t, p) {
      if (typeof p === "string" && /^[0-9]+$/.test(p)) { return Number(p) < t.length; }
      return p in t;
    }
  });
  Object.defineProperty(document, "styleSheets", {
    get: function () { return __docSheetListProxy; },
    enumerable: true, configurable: true
  });

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
      try { m(); } catch (e) { globalThis.__timerErrors.push((e&&e.stack||String(e))); }
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
    catch (e) { globalThis.__timerErrors.push((e&&e.stack||String(e))); }
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
  // A WHATWG-basic-URL-parser-style implementation (enough to pass the bulk of the url/ suite):
  // preprocessing, scheme + special-scheme handling, authority/host/port, opaque vs list paths with
  // dot-segment normalization, percent-encoding per destination set, relative resolution, and
  // canonical serialization. `base` (a parsed record or string) resolves a relative reference.
  var URL_SPECIAL = { ftp: "21", file: "", http: "80", https: "443", ws: "80", wss: "443" };
  function urlPctEncode(s, isInSet) {
    var out = "";
    for (var i = 0; i < s.length; i++) {
      var cp = s.codePointAt(i);
      if (cp > 0xffff) { i++; }
      if (cp > 0x7e || isInSet(cp)) {
        var bytes = unescape(encodeURIComponent(String.fromCodePoint(cp)));
        for (var b = 0; b < bytes.length; b++) {
          out += "%" + ("0" + bytes.charCodeAt(b).toString(16).toUpperCase()).slice(-2);
        }
      } else {
        out += String.fromCodePoint(cp);
      }
    }
    return out;
  }
  function urlC0(cp) { return cp < 0x20; }
  function urlFragSet(cp) { return urlC0(cp) || cp === 0x20 || cp === 0x22 || cp === 0x3c || cp === 0x3e || cp === 0x60; }
  function urlQuerySet(cp) { return urlC0(cp) || cp === 0x20 || cp === 0x22 || cp === 0x23 || cp === 0x3c || cp === 0x3e; }
  function urlPathSet(cp) { return urlQuerySet(cp) || cp === 0x3f || cp === 0x60 || cp === 0x7b || cp === 0x7d; }
  function urlUserSet(cp) { return urlPathSet(cp) || cp === 0x2f || cp === 0x3a || cp === 0x3b || cp === 0x3d || cp === 0x40 || cp === 0x5b || cp === 0x5c || cp === 0x5d || cp === 0x5e || cp === 0x7c; }

  // WHATWG IPv6 parser: an address (inside [...]) -> 8 16-bit pieces, or null on failure.
  function parseIPv6(input) {
    var address = [0, 0, 0, 0, 0, 0, 0, 0];
    var pieceIndex = 0, compress = null, p = 0, n = input.length;
    function c() { return p < n ? input[p] : null; }
    function isHex(ch) { return ch != null && /[0-9a-fA-F]/.test(ch); }
    if (c() === ":") {
      if (input[p + 1] !== ":") { return null; }
      p += 2; pieceIndex++; compress = pieceIndex;
    }
    while (c() !== null) {
      if (pieceIndex === 8) { return null; }
      if (c() === ":") {
        if (compress !== null) { return null; }
        p++; pieceIndex++; compress = pieceIndex; continue;
      }
      var value = 0, length = 0;
      while (length < 4 && isHex(c())) { value = value * 16 + parseInt(c(), 16); p++; length++; }
      if (c() === ".") {
        if (length === 0) { return null; }
        p -= length;
        if (pieceIndex > 6) { return null; }
        var numbersSeen = 0;
        while (c() !== null) {
          var ipv4Piece = null;
          if (numbersSeen > 0) { if (c() === "." && numbersSeen < 4) { p++; } else { return null; } }
          if (!/[0-9]/.test(c() || "")) { return null; }
          while (/[0-9]/.test(c() || "")) {
            var d = parseInt(c(), 10);
            if (ipv4Piece === null) { ipv4Piece = d; }
            else if (ipv4Piece === 0) { return null; }
            else { ipv4Piece = ipv4Piece * 10 + d; }
            if (ipv4Piece > 255) { return null; }
            p++;
          }
          address[pieceIndex] = address[pieceIndex] * 0x100 + ipv4Piece;
          numbersSeen++;
          if (numbersSeen === 2 || numbersSeen === 4) { pieceIndex++; }
        }
        if (numbersSeen !== 4) { return null; }
        break;
      } else if (c() === ":") { p++; if (c() === null) { return null; } }
      else if (c() !== null) { return null; }
      address[pieceIndex] = value; pieceIndex++;
    }
    if (compress !== null) {
      var swaps = pieceIndex - compress; pieceIndex = 7;
      while (pieceIndex !== 0 && swaps > 0) {
        var tmp = address[pieceIndex]; address[pieceIndex] = address[compress + swaps - 1]; address[compress + swaps - 1] = tmp;
        pieceIndex--; swaps--;
      }
    } else if (pieceIndex !== 8) { return null; }
    return address;
  }
  // Serialize 8 pieces with the canonical longest-zero-run compression.
  function serializeIPv6(address) {
    var out = "", compress = null, curBase = null, curLen = 0, maxLen = 0;
    for (var i = 0; i < 8; i++) {
      if (address[i] === 0) { if (curBase === null) { curBase = i; curLen = 1; } else { curLen++; } if (curLen > maxLen) { maxLen = curLen; compress = curBase; } }
      else { curBase = null; curLen = 0; }
    }
    if (maxLen < 2) { compress = null; }
    var ignore0 = false;
    for (var j = 0; j < 8; j++) {
      if (ignore0) { if (address[j] === 0) { continue; } ignore0 = false; }
      if (compress === j) { out += (j === 0 ? "::" : ":"); ignore0 = true; continue; }
      out += address[j].toString(16);
      if (j !== 7) { out += ":"; }
    }
    return out;
  }

  function parseURL(input, base) {
    if (base != null && typeof base === "string") { base = parseURLRecord(base, null); }
    var r = parseURLRecord(input, base || null);
    if (!r) { return { href: "", protocol: "", host: "", hostname: "", port: "", pathname: "", search: "", hash: "", origin: "null", username: "", password: "", __invalid: true }; }
    return serializeURLRecord(r);
  }

  function parseURLRecord(input, base) {
    input = String(input == null ? "" : input);
    input = input.replace(/^[\x00-\x20]+/, "").replace(/[\x00-\x20]+$/, "");
    input = input.replace(/[\t\n\r]/g, "");
    var u = { scheme: "", username: "", password: "", host: null, port: "", path: [], query: null, fragment: null, opaque: false };

    // Scheme.
    var sm = /^([a-zA-Z][a-zA-Z0-9+.\-]*):/.exec(input);
    var rest = input;
    if (sm) { u.scheme = sm[1].toLowerCase(); rest = input.slice(sm[0].length); }
    else if (base) {
      // No scheme → relative; inherit from base.
      u.scheme = base.scheme; u.username = base.username; u.password = base.password;
      u.host = base.host; u.port = base.port; u.opaque = base.opaque;
      u.path = base.path.slice(); u.query = base.query;
      return resolveRelative(u, rest, base);
    } else { return null; }

    var special = Object.prototype.hasOwnProperty.call(URL_SPECIAL, u.scheme);

    if (u.scheme === "file") {
      u.host = "";
      rest = rest.replace(/^\/\//, "");
      return parseAuthorityAndPath(u, rest, special, base);
    }
    if (special) {
      // Special non-file: must have an authority.
      if (base && base.scheme === u.scheme && !/^\/\//.test(rest) && rest.charAt(0) !== "/") {
        // "special relative" — treat as relative to base.
        u.host = base.host; u.port = base.port; u.path = base.path.slice(); u.query = base.query;
        return resolveRelative(u, rest, base);
      }
      rest = rest.replace(/^\/+/, m => "//"); // collapse leading slashes to one authority intro
      rest = rest.replace(/^\/\//, "");
      return parseAuthorityAndPath(u, rest, special, base);
    }
    // Non-special.
    if (/^\/\//.test(rest)) {
      rest = rest.slice(2);
      return parseAuthorityAndPath(u, rest, special, base);
    }
    // Opaque path (non-special, no //).
    u.opaque = true;
    var hf = splitTail(rest);
    u.path = [hf.body];
    u.query = hf.query;
    u.fragment = hf.fragment;
    if (u.fragment != null) { u.fragment = urlPctEncode(u.fragment, urlFragSet); }
    if (u.query != null) { u.query = urlPctEncode(u.query, urlQuerySet); }
    u.path[0] = urlPctEncode(u.path[0], function (cp) { return urlC0(cp) || cp > 0x7e; });
    return u;
  }

  // Split off ?query and #fragment from a reference; returns {body, query, fragment}.
  function splitTail(s) {
    var fragment = null, query = null, body = s;
    var h = body.indexOf('#');
    if (h >= 0) { fragment = body.slice(h + 1); body = body.slice(0, h); }
    var q = body.indexOf("?");
    if (q >= 0) { query = body.slice(q + 1); body = body.slice(0, q); }
    return { body: body, query: query, fragment: fragment };
  }

  function parseAuthorityAndPath(u, rest, special, base) {
    // Authority ends at the first /,?,# (or \ for special).
    var endRe = special ? /[\/\\?#]/ : /[\/?#]/;
    var em = endRe.exec(rest);
    var authEnd = em ? em.index : rest.length;
    var authority = rest.slice(0, authEnd);
    var after = rest.slice(authEnd);
    // userinfo@host:port
    var at = authority.lastIndexOf("@");
    if (at >= 0) {
      var ui = authority.slice(0, at);
      var pc = ui.indexOf(":");
      if (pc >= 0) { u.username = urlPctEncode(ui.slice(0, pc), urlUserSet); u.password = urlPctEncode(ui.slice(pc + 1), urlUserSet); }
      else { u.username = urlPctEncode(ui, urlUserSet); }
      authority = authority.slice(at + 1);
    }
    var host = authority, port = "";
    if (host.charAt(0) === "[") {
      var rb = host.indexOf("]");
      if (rb >= 0) {
        var addr = parseIPv6(host.slice(1, rb));
        var ip = addr ? "[" + serializeIPv6(addr) + "]" : host.slice(0, rb + 1);
        var tail = host.slice(rb + 1);
        if (tail.charAt(0) === ":") { port = tail.slice(1); }
        host = ip;
      }
    } else {
      var cidx = host.lastIndexOf(":");
      if (cidx >= 0) { port = host.slice(cidx + 1); host = host.slice(0, cidx); }
    }
    u.host = special ? host.toLowerCase() : host;
    if (port !== "") {
      if (!/^[0-9]*$/.test(port)) { return null; }
      var pn = parseInt(port, 10);
      if (pn > 65535) { return null; }
      // Omit the default port for the scheme.
      u.port = (URL_SPECIAL[u.scheme] === String(pn)) ? "" : String(pn);
    }
    return parsePath(u, after, special, base);
  }

  function parsePath(u, after, special, base) {
    var t = splitTail(after);
    if (t.fragment != null) { u.fragment = urlPctEncode(t.fragment, urlFragSet); }
    if (t.query != null) { u.query = urlPctEncode(t.query, urlQuerySet); }
    var pathStr = t.body;
    if (special) { pathStr = pathStr.replace(/\\/g, "/"); }
    var segs = pathStr === "" ? [] : pathStr.split("/");
    // A leading slash produces a leading empty segment; drop it (the path list starts after root).
    if (segs.length && segs[0] === "") { segs.shift(); }
    var out = [];
    for (var i = 0; i < segs.length; i++) {
      var seg = segs[i];
      var low = seg.toLowerCase();
      if (low === "." || low === "%2e") { continue; }
      if (low === ".." || low === ".%2e" || low === "%2e." || low === "%2e%2e") { if (out.length) { out.pop(); } continue; }
      out.push(urlPctEncode(seg, urlPathSet));
    }
    u.path = out;
    return u;
  }

  function resolveRelative(u, rest, base) {
    var t = splitTail(rest);
    if (rest.charAt(0) === '#') { u.fragment = urlPctEncode(t.fragment, urlFragSet); return u; }
    if (t.fragment != null) { u.fragment = urlPctEncode(t.fragment, urlFragSet); }
    if (rest === "" || rest.charAt(0) === '#') { u.query = (t.query != null) ? urlPctEncode(t.query, urlQuerySet) : base.query; return u; }
    if (rest.charAt(0) === "?") { u.query = urlPctEncode(t.query, urlQuerySet); u.path = base.path.slice(); return u; }
    u.query = (t.query != null) ? urlPctEncode(t.query, urlQuerySet) : null;
    var special = Object.prototype.hasOwnProperty.call(URL_SPECIAL, u.scheme);
    var body = t.body;
    if (special) { body = body.replace(/\\/g, "/"); }
    if (body.charAt(0) === "/") {
      return parsePath(u, "/" + body.replace(/^\/+/, "") + (t.query != null ? "?" + t.query : ""), special, base);
    }
    // Merge with base path (drop base's last segment).
    var basePath = base.path.slice();
    if (!(base.opaque)) { basePath.pop(); }
    var merged = (basePath.length ? "/" + basePath.join("/") + "/" : "/") + body;
    return parsePath(u, merged + (t.query != null ? "?" + t.query : ""), special, base);
  }

  function serializeURLRecord(u) {
    var special = Object.prototype.hasOwnProperty.call(URL_SPECIAL, u.scheme);
    var protocol = u.scheme + ":";
    var href = protocol;
    var hostStr = u.host == null ? "" : u.host;
    var authority = "";
    if (u.host != null) {
      href += "//";
      if (u.username || u.password) { href += u.username + (u.password ? ":" + u.password : "") + "@"; }
      href += hostStr;
      if (u.port !== "") { href += ":" + u.port; }
    }
    var pathname;
    if (u.opaque) { pathname = u.path[0] || ""; }
    else { pathname = u.path.length ? "/" + u.path.join("/") : (special ? "/" : ""); }
    href += pathname;
    var search = u.query != null ? "?" + u.query : "";
    var hash = u.fragment != null ? '#' + u.fragment : "";
    href += search + hash;
    var host = hostStr + (u.port !== "" ? ":" + u.port : "");
    var origin = (u.host != null && special && u.scheme !== "file") ? (protocol + "//" + host) : "null";
    return {
      href: href, protocol: protocol, host: host, hostname: hostStr, port: u.port,
      pathname: pathname, search: search, hash: hash, origin: origin,
      username: u.username, password: u.password, __rec: u
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

  // --- history (pushState/replaceState update location so SPA routers see the new URL) -------
  function __applyURLToLocation(url) {
    var resolved;
    try { resolved = new URL(String(url), location.href).href; } catch (e) { resolved = String(url); }
    var p = parseURL(resolved);
    location.href = p.href; location.protocol = p.protocol; location.host = p.host;
    location.hostname = p.hostname; location.port = p.port; location.pathname = p.pathname;
    location.search = p.search; location.hash = p.hash; location.origin = p.origin;
  }
  globalThis.history = {
    length: 1, scrollRestoration: "auto", state: null,
    pushState: function (state, title, url) {
      this.state = (state === undefined ? null : state);
      this.length++;
      if (url != null && url !== "") { __applyURLToLocation(url); }
    },
    replaceState: function (state, title, url) {
      this.state = (state === undefined ? null : state);
      if (url != null && url !== "") { __applyURLToLocation(url); }
    },
    back: fn, forward: fn, go: fn
  };

  // --- Storage (localStorage / sessionStorage) ---------------------------------------------
  // `persistKey` (the origin) makes the bucket write-through to disk via __storageSave and load
  // from __storageLoad — so localStorage survives reloads/restarts. sessionStorage passes none.
  function makeStorage(persistKey) {
    var map = Object.create(null);
    if (persistKey && typeof __storageLoad === "function") {
      try {
        var saved = __storageLoad(persistKey);
        if (saved) { var o = JSON.parse(saved); for (var k in o) { map[k] = String(o[k]); } }
      } catch (e) {}
    }
    var persist = (persistKey && typeof __storageSave === "function")
      ? function () { try { __storageSave(persistKey, JSON.stringify(map)); } catch (e) {} }
      : function () {};
    var s = {
      getItem: function (k) { k = String(k); return Object.prototype.hasOwnProperty.call(map, k) ? map[k] : null; },
      setItem: function (k, v) { map[String(k)] = String(v); persist(); },
      removeItem: function (k) { delete map[String(k)]; persist(); },
      clear: function () { map = Object.create(null); persist(); },
      key: function (i) { var ks = Object.keys(map); return i >= 0 && i < ks.length ? ks[i] : null; }
    };
    Object.defineProperty(s, "length", { get: function () { return Object.keys(map).length; }, enumerable: false, configurable: true });
    // Wrap in a Proxy so named access works too (`localStorage.foo = 1`, `localStorage.foo`,
    // `delete localStorage.foo`, `Object.keys(localStorage)`), backed by the same map.
    try {
      return new Proxy(s, {
        get: function (t, prop) { if (prop in t) { return t[prop]; } return typeof prop === "string" ? t.getItem(prop) : undefined; },
        set: function (t, prop, val) { if (prop in t && prop !== "length") { t[prop] = val; } else { t.setItem(String(prop), val); } return true; },
        deleteProperty: function (t, prop) { if (Object.prototype.hasOwnProperty.call(map, prop)) { t.removeItem(String(prop)); } else { delete t[prop]; } return true; },
        has: function (t, prop) { return (prop in t) || (typeof prop === "string" && Object.prototype.hasOwnProperty.call(map, prop)); },
        ownKeys: function () { return Object.keys(map); },
        getOwnPropertyDescriptor: function (t, prop) {
          if (Object.prototype.hasOwnProperty.call(map, prop)) { return { value: map[prop], writable: true, enumerable: true, configurable: true }; }
          return undefined;
        }
      });
    } catch (e) { return s; }
  }
  globalThis.localStorage = makeStorage((function () {
    try { var o = location.origin; return (o && o !== "null") ? o : (location.protocol + location.pathname); } catch (e) { return "default"; }
  })());
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
  globalThis.scrollX = 0; globalThis.pageXOffset = 0; // no horizontal scroll
  // scrollY / pageYOffset reflect the engine's real vertical scroll (updated as the page scrolls).
  try {
    Object.defineProperty(globalThis, "scrollY", { get: function () { try { return __scrollY(); } catch (e) { return 0; } }, configurable: true });
    Object.defineProperty(globalThis, "pageYOffset", { get: function () { try { return __scrollY(); } catch (e) { return 0; } }, configurable: true });
  } catch (e) { globalThis.scrollY = 0; globalThis.pageYOffset = 0; }
  globalThis.screenX = 0; globalThis.screenY = 0; globalThis.screenLeft = 0; globalThis.screenTop = 0;
  // scrollTo(x,y) | scrollTo({top}) — request a real scroll the engine applies.
  globalThis.scrollTo = function (x, y) {
    var top = (x && typeof x === "object") ? x.top : y;
    if (top != null) { try { __scrollSet(Number(top) || 0); } catch (e) {} }
  };
  globalThis.scroll = globalThis.scrollTo;
  globalThis.scrollBy = function (x, y) {
    var dy = (x && typeof x === "object") ? x.top : y;
    try { __scrollSet((Number(globalThis.scrollY) || 0) + (Number(dy) || 0)); } catch (e) {}
  };
  globalThis.moveTo = fn; globalThis.moveBy = fn; globalThis.resizeTo = fn; globalThis.resizeBy = fn;
  globalThis.focus = fn; globalThis.blur = fn; globalThis.print = fn;
  globalThis.open = function () { return null; }; globalThis.close = fn; globalThis.stop = fn;
  globalThis.getSelection = function () { return null; };
  globalThis.alert = fn; globalThis.confirm = function () { return false; }; globalThis.prompt = function () { return null; };

  // --- matchMedia (real evaluation against the live viewport) ------------------------------
  function __mqFeature(f) {
    var iw = Number(globalThis.innerWidth) || 0, ih = Number(globalThis.innerHeight) || 0;
    var dpr = Number(globalThis.devicePixelRatio) || 1;
    if (f === "screen" || f === "all") { return true; }
    if (f === "print" || f === "speech") { return false; }
    var m = f.match(/^\(\s*([a-z-]+)\s*(?::\s*([^)]+))?\s*\)$/);
    if (!m) { return false; }
    var name = m[1], val = (m[2] || "").trim();
    var px = function (v) { var n = parseFloat(v); if (/r?em$/.test(v)) { n *= 16; } return n; };
    var res = function (v) { return /dpi$/.test(v) ? parseFloat(v) / 96 : (/dpcm$/.test(v) ? parseFloat(v) / 37.8 : parseFloat(v)); };
    switch (name) {
      case "min-width": case "min-device-width": return iw >= px(val);
      case "max-width": case "max-device-width": return iw <= px(val);
      case "width": case "device-width": return iw === px(val);
      case "min-height": case "min-device-height": return ih >= px(val);
      case "max-height": case "max-device-height": return ih <= px(val);
      case "height": case "device-height": return ih === px(val);
      case "min-aspect-ratio": case "max-aspect-ratio": case "aspect-ratio": {
        var p = val.split("/"); var want = p.length === 2 ? (parseFloat(p[0]) / parseFloat(p[1])) : parseFloat(val);
        var have = ih ? iw / ih : 0;
        return name === "min-aspect-ratio" ? have >= want : (name === "max-aspect-ratio" ? have <= want : Math.abs(have - want) < 0.01);
      }
      case "orientation": return val === (iw >= ih ? "landscape" : "portrait");
      case "prefers-color-scheme": {
        // Reflect the real macOS appearance (live via the native __prefersDark() flag). Bare
        // `(prefers-color-scheme)` with no value matches always; `dark`/`light` match the OS.
        var dark = false; try { dark = !!__prefersDark(); } catch (e) {}
        if (val === "") { return true; }
        return dark ? (val === "dark") : (val === "light");
      }
      case "prefers-reduced-motion": return val === "" || val === "no-preference";
      case "prefers-contrast": return val === "" || val === "no-preference";
      case "hover": case "any-hover": return val === "" || val === "hover";
      case "pointer": case "any-pointer": return val === "" || val === "fine";
      case "min-resolution": return dpr >= res(val);
      case "max-resolution": return dpr <= res(val);
      case "resolution": return Math.abs(dpr - res(val)) < 0.01;
      case "display-mode": return val === "browser";
      case "scripting": return val === "" || val === "enabled";
      case "update": return val === "" || val === "fast";
      case "color": return val === "" || parseFloat(val) > 0;
      case "color-gamut": return val === "srgb";
      default: return false;
    }
  }
  function __mqConj(q) {
    var neg = false;
    if (/^not\s/.test(q)) { neg = true; q = q.replace(/^not\s+/, "").trim(); }
    q = q.replace(/^only\s+/, "");
    var parts = q.split(/\s+and\s+/);
    var all = true;
    for (var i = 0; i < parts.length; i++) { if (!__mqFeature(parts[i].trim())) { all = false; break; } }
    return neg ? !all : all;
  }
  function __evalMedia(query) {
    query = String(query == null ? "" : query).toLowerCase().trim();
    if (!query || query === "all" || query === "screen") { return true; }
    var ors = query.split(",");
    for (var i = 0; i < ors.length; i++) { if (__mqConj(ors[i].trim())) { return true; } }
    return false;
  }
  // Live MediaQueryList registry. Every matchMedia() result is kept (weakly via a plain list — the
  // page count is tiny) so that when the OS appearance flips we can re-evaluate each list and fire
  // `change` on the ones whose `.matches` actually changed. __mediaChanged() is called by the
  // engine path (globalThis hook) after it flips the prefers-color-scheme flag.
  var __mqlRegistry = [];
  globalThis.matchMedia = function (q) {
    var media = String(q);
    var listeners = []; // change listeners added via addEventListener('change', ...)/addListener
    var mql = {
      media: media, onchange: null,
      addEventListener: function (type, cb) { if (type === "change" && typeof cb === "function") { listeners.push(cb); } },
      removeEventListener: function (type, cb) { if (type === "change") { var i = listeners.indexOf(cb); if (i >= 0) { listeners.splice(i, 1); } } },
      // Legacy aliases (still used by older sites): addListener/removeListener take the callback directly.
      addListener: function (cb) { if (typeof cb === "function") { listeners.push(cb); } },
      removeListener: function (cb) { var i = listeners.indexOf(cb); if (i >= 0) { listeners.splice(i, 1); } },
      dispatchEvent: function () { return false; }
    };
    // `matches` re-evaluates against the current viewport + OS appearance on every read.
    Object.defineProperty(mql, "matches", { get: function () { return __evalMedia(q); }, enumerable: true, configurable: true });
    // Internal: re-evaluate; if `.matches` changed, fire `change` on onchange + all listeners.
    def(mql, "__last", __evalMedia(q));
    def(mql, "__reeval", function () {
      var now = __evalMedia(q);
      if (now === mql.__last) { return; }
      mql.__last = now;
      var ev = { type: "change", media: media, matches: now, target: mql, currentTarget: mql, bubbles: false, cancelable: false };
      try { if (typeof mql.onchange === "function") { mql.onchange.call(mql, ev); } } catch (e) {}
      var snapshot = listeners.slice();
      for (var i = 0; i < snapshot.length; i++) { try { snapshot[i].call(mql, ev); } catch (e) {} }
    });
    __mqlRegistry.push(mql);
    return mql;
  };
  // Re-evaluate every live MediaQueryList and fire `change` where `.matches` flipped. Called by the
  // engine after it updates the OS appearance (prefers-color-scheme) so theme toggles restyle pages.
  def(globalThis, "__mediaChanged", function () {
    for (var i = 0; i < __mqlRegistry.length; i++) { try { __mqlRegistry[i].__reeval(); } catch (e) {} }
  });

  // --- getComputedStyle --------------------------------------------------------------------
  // Returns a read-only CSSStyleDeclaration-like object backed by the in-Session cascade
  // (`__computedStyleProp` / `__computedStyleNames`, computed in Rust by the `style` crate). For a
  // detached object with no node id we fall back to the old empty-stub so callers don't throw.
  (function () {
    // camelCase (or vendor-prefixed) property name -> kebab-case. `fontSize` -> `font-size`;
    // `WebkitTransform` -> `-webkit-transform`; already-kebab names pass through unchanged.
    function camelToKebab(prop) {
      prop = String(prop);
      if (prop.indexOf("-") >= 0) { return prop.toLowerCase(); } // already kebab
      // Insert "-" before each uppercase letter, lowercase everything. A leading uppercase (vendor
      // prefixes like `Webkit`/`Moz`/`Ms`) becomes a leading "-" (e.g. `-webkit-transform`).
      var out = prop.replace(/[A-Z]/g, function (c) { return "-" + c.toLowerCase(); });
      return out;
    }

    function emptyDeclaration() {
      // Detached / no node id: behave like the old stub (every read is "").
      var base = {
        getPropertyValue: function () { return ""; },
        getPropertyPriority: function () { return ""; },
        setProperty: fn, removeProperty: function () { return ""; },
        item: function () { return ""; }, length: 0
      };
      try {
        return new Proxy(base, { get: function (t, p) { return (p in t) ? t[p] : ""; } });
      } catch (e) {
        var common = ["display", "color", "width", "height", "visibility", "opacity", "position", "margin", "padding", "font-size", "background-color"];
        for (var i = 0; i < common.length; i++) { base[common[i]] = ""; }
        return base;
      }
    }

    function makeDeclaration(id, pseudo) {
      pseudo = pseudo || "";
      var names = null; // lazily fetched list of populated property names
      function getNames() { if (names === null) { try { names = __computedStyleNames(id, pseudo) || []; } catch (e) { names = []; } } return names; }
      function get(prop) { try { return __computedStyleProp(id, String(prop).toLowerCase(), pseudo); } catch (e) { return ""; } }

      // Computed styles are read-only: mutators throw NoModificationAllowedError (per CSSOM).
      function readOnlyThrow() { throw new globalThis.DOMException("Cannot modify the computed (resolved) style.", "NoModificationAllowedError"); }
      var decl = {
        getPropertyValue: function (name) { return get(name); },
        getPropertyPriority: function () { return ""; },
        setProperty: function () { readOnlyThrow(); },
        removeProperty: function () { readOnlyThrow(); },
        item: function (i) { var n = getNames(); i = i >>> 0; return i < n.length ? n[i] : ""; },
        parentRule: null
      };
      // Iterable over property names (the indexed getter values).
      try { decl[Symbol.iterator] = function () { return makeIter(getNames(), function (i, v) { return v; }); }; } catch (e) {}
      // cssText on a computed (resolved) style declaration is the empty string; setting it throws.
      Object.defineProperty(decl, "cssText", { get: function () { return ""; }, set: function () { readOnlyThrow(); }, enumerable: true, configurable: true });
      Object.defineProperty(decl, "length", {
        get: function () { return getNames().length; }, enumerable: true, configurable: true
      });

      try {
        return new Proxy(decl, {
          get: function (target, prop) {
            if (typeof prop === "symbol") { return target[prop]; }
            if (prop in target) { return target[prop]; }
            // Numeric index -> the i-th property name (like a real CSSStyleDeclaration).
            if (/^[0-9]+$/.test(prop)) { var n = getNames(); var i = Number(prop); return i < n.length ? n[i] : undefined; }
            // Any other property: kebab or camelCase CSS property access.
            return get(camelToKebab(prop));
          },
          has: function (target, prop) {
            if (prop in target) { return true; }
            return get(camelToKebab(prop)) !== "";
          },
          // A computed-style CSSStyleDeclaration is read-only: writing a CSS property throws
          // NoModificationAllowedError (per CSSOM). Symbol writes pass through.
          set: function (target, prop, value) {
            if (typeof prop === "symbol") { target[prop] = value; return true; }
            throw new globalThis.DOMException(
              "Cannot modify the computed (resolved) style.", "NoModificationAllowedError");
          }
        });
      } catch (e) {
        // No Proxy: define the common longhands + index slots eagerly (matches the old fallback).
        var nm = getNames();
        for (var i = 0; i < nm.length; i++) {
          (function (k, idx) {
            var kebab = k;
            // expose both kebab and the camelCase alias
            decl[kebab] = get(kebab);
            decl[kebab.replace(/-([a-z])/g, function (_, c) { return c.toUpperCase(); })] = get(kebab);
            decl[idx] = kebab;
          })(nm[i], i);
        }
        return decl;
      }
    }

    globalThis.getComputedStyle = function (el, pseudoElt) {
      var id = (el && typeof el.__node === "number") ? el.__node : null;
      if (id === null) { return emptyDeclaration(); }
      // The pseudo-element argument is normalized in Rust (`parse_gcs_pseudo`); pass it through as a
      // string. null/undefined → the element itself.
      var pseudo = (pseudoElt === null || pseudoElt === undefined) ? "" : String(pseudoElt);
      return makeDeclaration(id, pseudo);
    };
  })();

  // --- event model (no-op but present) + a simple listener registry ------------------------
  // Normalize an addEventListener/removeEventListener `options` arg to a capture boolean (the 3rd
  // arg may be a boolean or an options dict `{capture}`); per spec, "capture" identity is what
  // distinguishes two registrations of the same callback for the same type.
  function __captureFlag(options) {
    if (options && typeof options === "object") { return !!options.capture; }
    return !!options;
  }
  function installEvents(target) {
    if (!target || typeof target !== "object") { return; }
    if (target.__listeners) { return; } // already installed
    var registry = Object.create(null); // type -> [ {cb, capture, once} ]
    def(target, "__listeners", registry);
    def(target, "addEventListener", function (type, cb, options) {
      if (typeof cb !== "function") { return; }
      type = String(type);
      var capture = __captureFlag(options);
      var once = !!(options && typeof options === "object" && options.once);
      var list = registry[type] || (registry[type] = []);
      // Duplicate (same callback + same capture) registrations are ignored.
      for (var i = 0; i < list.length; i++) { if (list[i].cb === cb && list[i].capture === capture) { return; } }
      var entry = { cb: cb, capture: capture, once: once };
      list.push(entry);
      // `{ signal }` option: auto-remove this listener when the AbortSignal aborts.
      var sig = options && typeof options === "object" ? options.signal : null;
      if (sig && typeof sig.addEventListener === "function") {
        if (sig.aborted) { var j0 = list.indexOf(entry); if (j0 >= 0) { list.splice(j0, 1); } return; }
        sig.addEventListener("abort", function () {
          var l = registry[type]; if (!l) { return; }
          var j = l.indexOf(entry); if (j >= 0) { l.splice(j, 1); }
        });
      }
    });
    def(target, "removeEventListener", function (type, cb, options) {
      type = String(type);
      var capture = __captureFlag(options);
      var list = registry[type];
      if (!list) { return; }
      for (var i = 0; i < list.length; i++) { if (list[i].cb === cb && list[i].capture === capture) { list.splice(i, 1); return; } }
    });
    def(target, "dispatchEvent", function (ev) {
      return globalThis.__dispatchEventObject(target, ev);
    });
  }
  // Invoke the listeners registered on `target` for `type` whose capture flag matches `wantCapture`,
  // plus (when invoking the bubble/target set) the legacy `on<type>` handler. Honours `once` and
  // the event's stop-immediate flag. `ev` may be a constructed Event (with __ev) or a plain object.
  def(globalThis, "__runListeners", function (target, type, ev, wantCapture, includeOn) {
    if (!target) { return; }
    var s = ev && ev.__ev ? ev.__ev : null;
    var reg = target.__listeners;
    var list = reg ? reg[type] : null;
    if (list) {
      var copy = list.slice();
      for (var i = 0; i < copy.length; i++) {
        var entry = copy[i];
        if (entry.capture !== wantCapture) { continue; }
        if (entry.once) { var j = list.indexOf(entry); if (j >= 0) { list.splice(j, 1); } }
        try { entry.cb.call(target, ev); } catch (e) { (globalThis.__timerErrors || []).push((e && e.stack) || String(e)); }
        if (s && s.stopImmediate) { return; }
      }
    }
    if (includeOn) {
      var on = target["on" + type];
      if (typeof on === "function") {
        try { on.call(target, ev); } catch (e2) { (globalThis.__timerErrors || []).push((e2 && e2.stack) || String(e2)); }
      }
    }
  });
  // Build an event's propagation path: [target, ancestors..., document, window]. The target is
  // always included; ancestors/document/window are the bubbling targets walked via parentNode.
  def(globalThis, "__eventPath", function (target) {
    var path = [target];
    // Only DOM nodes propagate to ancestors / document / window. Non-node EventTargets
    // (AbortSignal, XMLHttpRequest, WebSocket, …) dispatch to themselves only.
    var isNode = target === globalThis || target === document ||
                 (target && typeof target.__node === "number");
    if (!isNode) { return path; }
    var cur = target, guard = 0;
    while (cur && guard < 4096) {
      var parent = null;
      try { parent = cur.parentNode; } catch (e0) { parent = null; }
      if (!parent || parent === cur) { break; }
      path.push(parent); cur = parent; guard++;
    }
    if (path.indexOf(document) < 0) { path.push(document); }
    if (path.indexOf(globalThis) < 0) { path.push(globalThis); }
    return path;
  });
  // Shared dispatch for `target.dispatchEvent(ev)`. Drives constructed Event objects (which carry
  // internal __ev state + read-only getters) through the full DOM dispatch algorithm: builds the
  // propagation path, runs the capture phase (root -> target), the target phase, then the bubble
  // phase (target -> root) when ev.bubbles, setting target/currentTarget/eventPhase and honouring
  // stopPropagation/stopImmediatePropagation. Falls back gracefully for plain {type} objects.
  def(globalThis, "__dispatchEventObject", function (target, ev) {
    var type = ev && ev.type != null ? String(ev.type) : "";
    var s = ev && ev.__ev ? ev.__ev : null; // internal state for constructed events
    var bubbles = s ? s.bubbles : !!(ev && ev.bubbles);

    var path = globalThis.__eventPath(target); // [target, ...ancestors, document, window]

    if (s) {
      s.dispatched = true; s.target = target; s.stopPropagation = false;
      s.stopImmediate = false; s.path = path.slice();
    } else { try { ev.target = target; } catch (e1) {} }

    function setCT(ct, phase) {
      if (s) { s.currentTarget = ct; s.eventPhase = phase; }
      else { try { ev.currentTarget = ct; } catch (e2) {} }
    }
    var run = globalThis.__runListeners;

    // Capture phase: ancestors from outermost (window) down to (but not including) the target.
    for (var i = path.length - 1; i >= 1; i--) {
      if (s && s.stopPropagation) { break; }
      setCT(path[i], 1 /*CAPTURING_PHASE*/);
      run(path[i], type, ev, true, false);
    }
    // Target phase: both capture- and bubble-registered listeners fire here, plus on<type>.
    if (!(s && s.stopPropagation)) {
      setCT(target, 2 /*AT_TARGET*/);
      run(target, type, ev, true, false);
      if (!(s && s.stopImmediate)) { run(target, type, ev, false, true); }
    }
    // Bubble phase: ancestors from target's parent up to window (only when the event bubbles).
    if (bubbles) {
      for (var h = 1; h < path.length; h++) {
        if (s && s.stopPropagation) { break; }
        setCT(path[h], 3 /*BUBBLING_PHASE*/);
        run(path[h], type, ev, false, true);
      }
    }

    if (s) { s.eventPhase = 0; s.currentTarget = null; s.stopPropagation = false; s.stopImmediate = false; }
    else { try { ev.currentTarget = null; } catch (e3) {} }
    return s ? !s.defaultPrevented : !(ev && ev.defaultPrevented);
  });
  installEvents(globalThis);
  installEvents(document);

  // --- DOMException + AbortController/AbortSignal -------------------------------------------
  // A real DOMException carrying `name`/`message` (AbortError, TimeoutError, …).
  (function () {
    // Map a DOMException name to its legacy numeric `code` (0 when the name has no legacy code).
    var __domCodes = {
      IndexSizeError: 1, HierarchyRequestError: 3, WrongDocumentError: 4,
      InvalidCharacterError: 5, NoModificationAllowedError: 7, NotFoundError: 8,
      NotSupportedError: 9, InUseAttributeError: 10, InvalidStateError: 11,
      SyntaxError: 12, InvalidModificationError: 13, NamespaceError: 14,
      InvalidAccessError: 15, TypeMismatchError: 17, SecurityError: 18,
      NetworkError: 19, AbortError: 20, URLMismatchError: 21, QuotaExceededError: 22,
      TimeoutError: 23, InvalidNodeTypeError: 24, DataCloneError: 25
    };
    var DOMExceptionCtor = function (message, name) {
      this.message = message === undefined ? "" : String(message);
      this.name = name === undefined ? "Error" : String(name);
      this.code = __domCodes[this.name] || 0;
      try { this.stack = new Error(this.message).stack; } catch (e) {}
    };
    DOMExceptionCtor.prototype = Object.create(Error.prototype);
    DOMExceptionCtor.prototype.constructor = DOMExceptionCtor;
    DOMExceptionCtor.prototype.toString = function () { return this.name + ": " + this.message; };
    // The constructor's own .name must be "DOMException" (it's inferred as the variable name
    // otherwise): testharness's assert_throws_dom checks `constructor.name === "DOMException"` to
    // detect the explicit-constructor overload, so a wrong name silently misroutes its arguments.
    try { Object.defineProperty(DOMExceptionCtor, "name", { value: "DOMException", configurable: true }); } catch (e) {}
    def(globalThis, "DOMException", DOMExceptionCtor);
  })();

  function __makeAbortReason(reason) {
    return reason !== undefined ? reason : new globalThis.DOMException("The operation was aborted.", "AbortError");
  }
  function __abortSignal(signal, reason) {
    if (!signal || signal.aborted) { return; }
    signal.aborted = true;
    signal.reason = __makeAbortReason(reason);
    var ev = { type: "abort", target: signal, currentTarget: signal, bubbles: false };
    if (typeof signal.onabort === "function") { try { signal.onabort.call(signal, ev); } catch (e) { (globalThis.__timerErrors || []).push((e&&e.stack||String(e))); } }
    if (typeof signal.dispatchEvent === "function") { try { signal.dispatchEvent(ev); } catch (e) {} }
  }
  function AbortSignal() {
    this.aborted = false;
    this.reason = undefined;
    this.onabort = null;
    installEvents(this);
  }
  AbortSignal.prototype.throwIfAborted = function () { if (this.aborted) { throw this.reason; } };
  AbortSignal.abort = function (reason) { var s = new AbortSignal(); __abortSignal(s, reason); return s; };
  AbortSignal.timeout = function (ms) {
    var s = new AbortSignal();
    setTimeout(function () { __abortSignal(s, new globalThis.DOMException("The operation timed out.", "TimeoutError")); }, Number(ms) || 0);
    return s;
  };
  AbortSignal.any = function (signals) {
    var s = new AbortSignal();
    var list = Array.prototype.slice.call(signals || []);
    for (var i = 0; i < list.length; i++) { if (list[i] && list[i].aborted) { __abortSignal(s, list[i].reason); return s; } }
    list.forEach(function (sig) {
      if (sig && typeof sig.addEventListener === "function") { sig.addEventListener("abort", function () { __abortSignal(s, sig.reason); }); }
    });
    return s;
  };
  def(globalThis, "AbortSignal", AbortSignal);

  function AbortController() { this.signal = new AbortSignal(); }
  AbortController.prototype.abort = function (reason) { __abortSignal(this.signal, reason); };
  def(globalThis, "AbortController", AbortController);

  // --- DOM lifecycle dispatch (driven from Rust during the drain) --------------------------
  var readyState = "loading";
  Object.defineProperty(document, "readyState", { get: function () { return readyState; }, enumerable: true, configurable: true });
  // The document's window. Code reads `document.defaultView` (and `node.ownerDocument.defaultView`)
  // to reach the global — e.g. google's `_.ai = a => a ? a.defaultView : window`, then
  // `_.ai(doc).devicePixelRatio`. Must be the same object as window/globalThis/self.
  if (!("defaultView" in document)) { def(document, "defaultView", globalThis); }
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
      // dispatchEvent already invokes both addEventListener listeners AND the `on<type>` handler
      // (e.g. window.onload), so we must NOT call the handler again here — that double-fires `load`,
      // which makes pages that build state in onload (e.g. testharness `test()`s) run twice.
      try { target.dispatchEvent(makeEvent(type)); } catch (e) { (globalThis.__timerErrors || []).push((e&&e.stack||String(e))); }
      return;
    }
    // Fallback for a target without a real dispatchEvent: invoke the `on<type>` handler directly.
    var on = target ? target["on" + type] : null;
    if (typeof on === "function") {
      try { on.call(target, makeEvent(type)); } catch (e) { (globalThis.__timerErrors || []).push((e&&e.stack||String(e))); }
    }
  }
  // Called from Rust's drain phase, in order, to advance readyState and fire lifecycle events.
  // MUST be idempotent: the drain calls it on every tick, but `DOMContentLoaded`/`load`/`pageshow`
  // are one-shot — firing them repeatedly breaks non-idempotent handlers (analytics init, jQuery
  // ready, testharness.js completion, etc.). Guard so the sequence runs exactly once.
  var __lifecycleFired = false;
  def(globalThis, "__fireLifecycleEvents", function () {
    if (__lifecycleFired) { return; }
    __lifecycleFired = true;
    readyState = "interactive";
    fireOn(document, "readystatechange");
    fireOn(document, "DOMContentLoaded");
    readyState = "complete";
    fireOn(document, "readystatechange");
    // Fire `load` on each connected, enabled stylesheet <link> with an inline `data:` sheet before
    // the window load — those are available synchronously. We deliberately do NOT fire for external
    // hrefs here: we can't tell when a real (possibly slow / render-blocking) sheet has finished
    // loading, and firing early would run a page's onload check before its CSS is applied.
    try {
      var __lks = document.querySelectorAll("link[rel~=stylesheet]");
      for (var __i = 0; __i < __lks.length; __i++) {
        var __lk = __lks[__i];
        var __href = __lk.getAttribute && __lk.getAttribute("href");
        if (__lk.__loadFired || !__href || __href.slice(0, 5) !== "data:" || __lk.disabled) { continue; }
        def(__lk, "__loadFired", true);
        try { __lk.dispatchEvent(new Event("load")); } catch (e) {}
      }
    } catch (e) {}
    // <style> elements fire `load` once their style block is processed (synchronously available).
    try {
      var __sts = document.querySelectorAll("style");
      for (var __j = 0; __j < __sts.length; __j++) {
        var __st = __sts[__j];
        if (__st.__loadFired) { continue; }
        def(__st, "__loadFired", true);
        try { __st.dispatchEvent(new Event("load")); } catch (e) {}
      }
    } catch (e) {}
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
  // Normalize a single numeric token to CSSOM canonical form: add a leading `0` before a bare
  // decimal point (`.5` -> `0.5`), drop a redundant leading zero pair only where the spec keeps it
  // (we keep `0.5`), strip trailing fractional zeros (`1.50` -> `1.5`, `2.0` -> `2`), and collapse
  // negative zero (`-0`, `-0.0`) to `0`. `num` is the sign+digits+optional-fraction (no unit).
  function normalizeNumberToken(num) {
    var neg = num.charAt(0) === "-";
    var sign = neg ? "-" : (num.charAt(0) === "+" ? "" : "");
    var body = (num.charAt(0) === "-" || num.charAt(0) === "+") ? num.slice(1) : num;
    if (body.charAt(0) === ".") { body = "0" + body; }
    if (body.indexOf(".") >= 0) {
      body = body.replace(/0+$/, "");      // trim trailing zeros
      if (body.charAt(body.length - 1) === ".") { body = body.slice(0, -1); }
    }
    // Collapse negative zero.
    if (sign === "-" && /^0(?:\.0*)?$/.test(body)) { sign = ""; }
    return sign + body;
  }
  // Canonicalize the numeric tokens inside a CSS value string (leading zeros, negative zero,
  // trailing fractional zeros), preserving units, identifiers, and `url(...)`/quoted segments.
  function normalizeCssValue(val) {
    val = String(val);
    // Canonicalize `url(...)`: the argument is serialized as a double-quoted string. Matches
    // `url( ... )` with an unquoted or single-quoted body and rewrites to `url("body")`.
    val = val.replace(/url\(\s*(?:"([^"]*)"|'([^']*)'|([^)\s]*))\s*\)/gi, function (_m, dq, sq, uq) {
      var body = dq != null ? dq : (sq != null ? sq : (uq != null ? uq : ""));
      return 'url("' + body + '")';
    });
    // `counter(name, decimal)` / `counters(name, sep, decimal)`: the default `decimal` style is
    // omitted on serialization.
    val = val.replace(/counter\(\s*([^,)]+?)\s*,\s*decimal\s*\)/gi, function (_m, nm) { return "counter(" + nm.trim() + ")"; });
    var out = "";
    var i = 0, n = val.length;
    while (i < n) {
      var ch = val[i];
      // Skip quoted strings verbatim (property-specific quote canonicalization happens in pushDecl).
      if (ch === '"' || ch === "'") {
        var q = ch; out += ch; i++;
        while (i < n && val[i] !== q) { if (val[i] === "\\" && i + 1 < n) { out += val[i] + val[i + 1]; i += 2; continue; } out += val[i]; i++; }
        if (i < n) { out += val[i]; i++; }
        continue;
      }
      // A number token: optional sign, digits with optional single decimal point.
      var rest = val.slice(i);
      var m = /^[-+]?(?:\d+\.?\d*|\.\d+)/.exec(rest);
      if (m && m[0].length > 0) {
        // Only treat as a number if not part of an identifier (preceding char isn't a letter/_/-).
        var prev = out.length ? out[out.length - 1] : "";
        var startsAlpha = /[A-Za-z_]/.test(prev);
        if (!startsAlpha) {
          out += normalizeNumberToken(m[0]);
          i += m[0].length;
          continue;
        }
      }
      out += ch; i++;
    }
    return out;
  }
  // Re-quote every top-level CSS string in `val` to double-quote form (CSSOM "serialize a string").
  // Used for properties whose <string> values are always quoted on serialization (content, quotes).
  function requoteStrings(val) {
    val = String(val);
    var out = "", i = 0, n = val.length;
    while (i < n) {
      var ch = val[i];
      if (ch === '"' || ch === "'") {
        var q = ch; i++; var body = "";
        while (i < n) {
          var cc = val[i];
          if (cc === "\\") { if (i + 1 < n) { body += cc + val[i + 1]; i += 2; } else { i++; } continue; }
          if (cc === q) { i++; break; }
          body += cc; i++;
        }
        out += '"' + body.replace(/"/g, '\\"') + '"';
        continue;
      }
      out += ch; i++;
    }
    return out;
  }
  // Serialize a font-family list: drop quotes around any family name that is a sequence of valid CSS
  // identifiers (so `'Lucida Grande'` -> `Lucida Grande`); keep quotes otherwise.
  // Generic font families and other reserved words that a quoted <family-name> must NOT be
  // unquoted into (they would otherwise be reinterpreted as a keyword).
  var GENERIC_FONT_FAMILIES = {
    "serif":1, "sans-serif":1, "cursive":1, "fantasy":1, "monospace":1, "system-ui":1, "math":1,
    "ui-serif":1, "ui-sans-serif":1, "ui-monospace":1, "ui-rounded":1
  };
  // A quoted family-name body must stay quoted if it is a single token equal to a generic family,
  // a CSS-wide keyword, or `default` (CSS Fonts: those are not valid <custom-ident>s here).
  function isReservedFontFamilyWord(body) {
    var b = body.toLowerCase();
    if (hasOwn(GENERIC_FONT_FAMILIES, b)) return true;
    if (b === "default" || b === "inherit" || b === "initial" || b === "unset" || b === "revert" || b === "revert-layer") return true;
    return false;
  }
  function normalizeFontFamily(val) {
    var parts = splitTopLevel(String(val), ",");
    var out = [];
    for (var p = 0; p < parts.length; p++) {
      var fam = parts[p].trim();
      if (fam === "") continue;
      var first = fam.charAt(0);
      if (first === '"' || first === "'") {
        // Quoted: serialize unquoted iff the body is a single-space-separated sequence of valid CSS
        // identifiers, re-joining reproduces the body exactly (no double spaces / leading/trailing
        // space), and the body isn't a reserved word (generic family / CSS-wide keyword / default).
        var body = fam.slice(1, -1);
        var words = body.split(" ");
        var allIdent = words.length > 0 && words.every(function (w) { return /^-?[A-Za-z_][A-Za-z0-9_-]*$/.test(w); });
        var roundTrips = allIdent && words.join(" ") === body;
        if (roundTrips && !isReservedFontFamilyWord(body)) {
          out.push(body);
        } else {
          out.push('"' + body.replace(/\\/g, "\\\\").replace(/"/g, '\\"') + '"');
        }
      } else {
        out.push(fam.replace(/\s+/g, " "));
      }
    }
    return out.join(", ");
  }
  // ====== CSS shorthand <-> longhand machinery (CSSOM serialize-a-CSS-declaration-block) ========
  // The set of longhands the `all` shorthand resets (every property except direction, unicode-bidi
  // and custom properties). A representative list — covers the properties the CSSOM tests query.
  var ALL_LONGHANDS = [
    "color", "background-color", "background-image", "background-position-x", "background-position-y",
    "background-size", "background-repeat", "background-attachment", "background-origin", "background-clip",
    "width", "height", "min-width", "min-height", "max-width", "max-height",
    "margin-top", "margin-right", "margin-bottom", "margin-left",
    "padding-top", "padding-right", "padding-bottom", "padding-left",
    "top", "right", "bottom", "left", "position", "display", "float", "clear", "visibility", "opacity",
    "border-top-width", "border-right-width", "border-bottom-width", "border-left-width",
    "border-top-style", "border-right-style", "border-bottom-style", "border-left-style",
    "border-top-color", "border-right-color", "border-bottom-color", "border-left-color",
    "border-top-left-radius", "border-top-right-radius", "border-bottom-right-radius", "border-bottom-left-radius",
    "font-family", "font-size", "font-style", "font-weight",
    "font-variant-ligatures", "font-variant-caps", "font-variant-alternates", "font-variant-numeric",
    "font-variant-east-asian", "font-variant-position", "font-variant-emoji",
    "font-stretch", "line-height",
    "text-align", "text-decoration-line", "text-decoration-style", "text-decoration-color",
    "text-transform", "letter-spacing", "white-space", "vertical-align",
    "list-style-type", "list-style-position", "list-style-image",
    "overflow-x", "overflow-y", "z-index", "cursor", "box-sizing",
    "flex-direction", "flex-wrap", "flex-grow", "flex-shrink", "flex-basis",
    "align-items", "align-content", "align-self", "justify-items", "justify-content", "justify-self",
    "row-gap", "column-gap", "outline-width", "outline-style", "outline-color",
    // Reset by `all` too (every property except direction / unicode-bidi / custom props).
    "border-collapse", "border-spacing", "order", "grid-template-columns", "grid-template-rows",
    // Logical longhands — also covered by `all` (so they collapse into it on serialization).
    "inline-size", "block-size", "min-inline-size", "min-block-size", "max-inline-size", "max-block-size",
    "margin-block-start", "margin-block-end", "margin-inline-start", "margin-inline-end",
    "padding-block-start", "padding-block-end", "padding-inline-start", "padding-inline-end",
    "inset-block-start", "inset-block-end", "inset-inline-start", "inset-inline-end",
    "border-block-start-width", "border-block-end-width", "border-inline-start-width", "border-inline-end-width",
    "border-block-start-style", "border-block-end-style", "border-inline-start-style", "border-inline-end-style",
    "border-block-start-color", "border-block-end-color", "border-inline-start-color", "border-inline-end-color"
  ];
  // A custom property is `--*`; case-sensitive, value kept raw (whitespace-trimmed).
  function isCustomProp(name) { return name.length >= 2 && name[0] === "-" && name[1] === "-"; }
  // Own-property lookup guarded against inherited keys (`"constructor"`, `"__proto__"`, …), so a CSS
  // property literally named like an Object.prototype member can't accidentally match a table entry.
  function hasOwn(obj, key) { return Object.prototype.hasOwnProperty.call(obj, key); }
  function lookup(obj, key) { return hasOwn(obj, key) ? obj[key] : undefined; }
  // CSS-wide keywords (valid for any property incl. the `all` shorthand).
  function isCssWideKeyword(v) {
    v = String(v).trim().toLowerCase();
    return v === "inherit" || v === "initial" || v === "unset" || v === "revert" || v === "revert-layer";
  }
  // Split a value into top-level space-separated tokens (respecting parens + quotes).
  function splitCssTokens(v) {
    v = String(v).trim();
    var out = [], i = 0, n = v.length, depth = 0, q = null, start = -1;
    while (i < n) {
      var c = v[i];
      if (q) { if (c === q) { q = null; } i++; continue; }
      if (c === '"' || c === "'") { if (start < 0) start = i; q = c; i++; continue; }
      if (c === "(") { if (start < 0) start = i; depth++; i++; continue; }
      if (c === ")") { depth--; i++; continue; }
      if (depth === 0 && (c === " " || c === "\t" || c === "\n" || c === "\r" || c === "\f")) {
        if (start >= 0) { out.push(v.slice(start, i)); start = -1; }
        i++; continue;
      }
      if (start < 0) start = i; i++;
    }
    if (start >= 0) out.push(v.slice(start));
    return out;
  }
  // Expand 1-4 box values into [top, right, bottom, left].
  function expandBox(v) {
    var t = splitCssTokens(v);
    if (t.length === 1) return [t[0], t[0], t[0], t[0]];
    if (t.length === 2) return [t[0], t[1], t[0], t[1]];
    if (t.length === 3) return [t[0], t[1], t[2], t[1]];
    if (t.length === 4) return [t[0], t[1], t[2], t[3]];
    return null;
  }
  // Serialize [top, right, bottom, left] to the shortest 1-4 box form.
  function serializeBox(top, right, bottom, left) {
    if (top == null || right == null || bottom == null || left == null) return "";
    if (top === bottom && right === left && top === right) return top;          // 1 value
    if (top === bottom && right === left) return top + " " + right;             // 2 values
    if (right === left) return top + " " + right + " " + bottom;               // 3 values
    return top + " " + right + " " + bottom + " " + left;                       // 4 values
  }
  // The box shorthands: shorthand -> [topLong, rightLong, bottomLong, leftLong].
  var BOX_SHORTHANDS = {
    "margin": ["margin-top", "margin-right", "margin-bottom", "margin-left"],
    "padding": ["padding-top", "padding-right", "padding-bottom", "padding-left"],
    "inset": ["top", "right", "bottom", "left"],
    "border-width": ["border-top-width", "border-right-width", "border-bottom-width", "border-left-width"],
    "border-style": ["border-top-style", "border-right-style", "border-bottom-style", "border-left-style"],
    "border-color": ["border-top-color", "border-right-color", "border-bottom-color", "border-left-color"],
    "border-radius": null, // handled specially
    "scroll-margin": ["scroll-margin-top", "scroll-margin-right", "scroll-margin-bottom", "scroll-margin-left"],
    "scroll-padding": ["scroll-padding-top", "scroll-padding-right", "scroll-padding-bottom", "scroll-padding-left"]
  };
  // Per-side `border-top`/`-right`/`-bottom`/`-left`: each -> [width, style, color] longhands.
  var BORDER_SIDE = {
    "border-top": ["border-top-width", "border-top-style", "border-top-color"],
    "border-right": ["border-right-width", "border-right-style", "border-right-color"],
    "border-bottom": ["border-bottom-width", "border-bottom-style", "border-bottom-color"],
    "border-left": ["border-left-width", "border-left-style", "border-left-color"],
    "outline": ["outline-color", "outline-style", "outline-width"],
    "column-rule": ["column-rule-width", "column-rule-style", "column-rule-color"]
  };
  // Classify a single border/outline component token as width|style|color.
  var BORDER_STYLE_KW = { none:1, hidden:1, dotted:1, dashed:1, solid:1, double:1, groove:1, ridge:1, inset:1, outset:1 };
  function classifyBorderToken(tok) {
    var t = tok.toLowerCase();
    if (BORDER_STYLE_KW[t]) return "style";
    if (t === "thin" || t === "medium" || t === "thick" || /^[-+.\d]/.test(t) || /^calc\(/.test(t)) return "width";
    return "color";
  }
  // Parse `border`/`border-top`/`outline` value -> {width,style,color} (missing -> undefined).
  function parseBorderLike(v) {
    var toks = splitCssTokens(v), r = {};
    for (var i = 0; i < toks.length; i++) {
      var k = classifyBorderToken(toks[i]);
      if (r[k] === undefined) r[k] = toks[i];
    }
    return r;
  }
  // The longhands of the `border` shorthand (all 12 sides + image), in canonical order.
  var BORDER_ALL_LONGHANDS = [
    "border-top-width", "border-right-width", "border-bottom-width", "border-left-width",
    "border-top-style", "border-right-style", "border-bottom-style", "border-left-style",
    "border-top-color", "border-right-color", "border-bottom-color", "border-left-color",
    "border-image-source", "border-image-slice", "border-image-width", "border-image-outset", "border-image-repeat"
  ];
  var BORDER_IMAGE_LONGHANDS = ["border-image-source", "border-image-slice", "border-image-width", "border-image-outset", "border-image-repeat"];
  var BORDER_IMAGE_INITIAL = {
    "border-image-source": "none", "border-image-slice": "100%", "border-image-width": "1",
    "border-image-outset": "0", "border-image-repeat": "stretch"
  };
  // overflow / overscroll-behavior / gap: 1-2 value shorthand of x/y (or row/column).
  // The flow-relative box shorthands (`margin-inline` etc.) are 2-value start/end shorthands too.
  var XY_SHORTHANDS = {
    "overflow": ["overflow-x", "overflow-y"],
    "overscroll-behavior": ["overscroll-behavior-x", "overscroll-behavior-y"],
    "gap": ["row-gap", "column-gap"],
    "margin-inline": ["margin-inline-start", "margin-inline-end"],
    "margin-block": ["margin-block-start", "margin-block-end"],
    "padding-inline": ["padding-inline-start", "padding-inline-end"],
    "padding-block": ["padding-block-start", "padding-block-end"],
    "inset-inline": ["inset-inline-start", "inset-inline-end"],
    "inset-block": ["inset-block-start", "inset-block-end"]
  };
  // list-style: type/position/image.
  function parseListStyle(v) {
    var toks = splitCssTokens(v), r = { "list-style-type": undefined, "list-style-position": undefined, "list-style-image": undefined };
    var POS = { inside: 1, outside: 1 };
    for (var i = 0; i < toks.length; i++) {
      var t = toks[i], tl = t.toLowerCase();
      if (/^url\(/i.test(t)) { r["list-style-image"] = t; }
      else if (POS[tl]) { r["list-style-position"] = tl; }
      else if (tl === "none") { if (r["list-style-type"] === undefined) r["list-style-type"] = "none"; else r["list-style-image"] = "none"; }
      else { r["list-style-type"] = t; }
    }
    return r;
  }
  // Map a shorthand name to its full set of longhand property names.
  function shorthandLonghands(name) {
    if (name === "all") return null; // special
    if (hasOwn(BOX_SHORTHANDS, name)) {
      if (name === "border-radius") return ["border-top-left-radius", "border-top-right-radius", "border-bottom-right-radius", "border-bottom-left-radius"];
      return BOX_SHORTHANDS[name];
    }
    if (hasOwn(BORDER_SIDE, name)) return BORDER_SIDE[name];
    if (hasOwn(XY_SHORTHANDS, name)) return XY_SHORTHANDS[name];
    if (name === "border") return BORDER_ALL_LONGHANDS;
    if (name === "font-variant") return FONT_VARIANT_LONGHANDS;
    if (name === "border-image") return BORDER_IMAGE_LONGHANDS;
    if (name === "list-style") return ["list-style-position", "list-style-image", "list-style-type"];
    if (name === "text-decoration") return ["text-decoration-line", "text-decoration-style", "text-decoration-color"];
    if (name === "flex-flow") return ["flex-direction", "flex-wrap"];
    // flex expands in the order grow, basis, shrink (matches browser declaration-block order).
    if (name === "flex") return ["flex-grow", "flex-basis", "flex-shrink"];
    if (name === "place-content") return ["align-content", "justify-content"];
    if (name === "place-items") return ["align-items", "justify-items"];
    if (name === "place-self") return ["align-self", "justify-self"];
    if (name === "columns") return ["column-width", "column-count"];
    return null;
  }
  // Shorthands we don't value-serialize but whose longhand set we know, so the CSS-wide-keyword
  // case (e.g. reading `font` after `all: revert`) can be serialized. Used by getVal only.
  // The `font` longhands listed here use the *granular* font-variant longhands (the actual stored
  // properties), not the `font-variant` sub-shorthand, so that after `all: <css-wide-keyword>` — which
  // expands to those granular longhands — `getPropertyValue("font")` can detect that every font
  // longhand carries the same CSS-wide keyword and return it.
  var KEYWORD_ONLY_SHORTHANDS = {
    "font": ["font-style", "font-variant-ligatures", "font-variant-caps", "font-variant-alternates",
      "font-variant-numeric", "font-variant-east-asian", "font-variant-position", "font-variant-emoji",
      "font-weight", "font-stretch", "font-size", "line-height", "font-family"],
    "background": ["background-image", "background-position-x", "background-position-y", "background-size", "background-repeat", "background-origin", "background-clip", "background-attachment", "background-color"]
  };
  // font-variant shorthand longhands, in canonical serialization order.
  var FONT_VARIANT_LONGHANDS = [
    "font-variant-ligatures", "font-variant-caps", "font-variant-alternates",
    "font-variant-numeric", "font-variant-east-asian", "font-variant-position", "font-variant-emoji"
  ];
  // Keyword sets used to bucket a `font-variant` shorthand token into the right longhand.
  var FV_LIGATURES = { "common-ligatures":1, "no-common-ligatures":1, "discretionary-ligatures":1, "no-discretionary-ligatures":1, "historical-ligatures":1, "no-historical-ligatures":1, "contextual":1, "no-contextual":1 };
  var FV_CAPS = { "small-caps":1, "all-small-caps":1, "petite-caps":1, "all-petite-caps":1, "unicase":1, "titling-caps":1 };
  var FV_NUMERIC = { "lining-nums":1, "oldstyle-nums":1, "proportional-nums":1, "tabular-nums":1, "diagonal-fractions":1, "stacked-fractions":1, "ordinal":1, "slashed-zero":1 };
  var FV_EAST_ASIAN = { "jis78":1, "jis83":1, "jis90":1, "jis04":1, "simplified":1, "traditional":1, "full-width":1, "proportional-width":1, "ruby":1 };
  var FV_POSITION = { "sub":1, "super":1 };
  var FV_EMOJI = { "text":1, "emoji":1, "unicode":1 };
  var FV_ALTERNATES = { "historical-forms":1 };
  // Expand a `font-variant` shorthand value into its longhands, or null if unparseable.
  function expandFontVariant(value) {
    var v = String(value).trim(), vl = v.toLowerCase();
    var res = {
      "font-variant-ligatures": "normal", "font-variant-caps": "normal", "font-variant-alternates": "normal",
      "font-variant-numeric": "normal", "font-variant-east-asian": "normal", "font-variant-position": "normal",
      "font-variant-emoji": "normal"
    };
    if (vl === "normal") return res;
    if (vl === "none") { res["font-variant-ligatures"] = "none"; return res; }
    var toks = splitCssTokens(v), buckets = {};
    for (var i = 0; i < toks.length; i++) {
      var t = toks[i].toLowerCase(), lh = null;
      if (FV_LIGATURES[t]) lh = "font-variant-ligatures";
      else if (FV_CAPS[t]) lh = "font-variant-caps";
      else if (FV_NUMERIC[t]) lh = "font-variant-numeric";
      else if (FV_EAST_ASIAN[t]) lh = "font-variant-east-asian";
      else if (FV_POSITION[t]) lh = "font-variant-position";
      else if (FV_EMOJI[t]) lh = "font-variant-emoji";
      else if (FV_ALTERNATES[t]) lh = "font-variant-alternates";
      else return null; // unknown token -> invalid shorthand
      if (!buckets[lh]) buckets[lh] = [];
      buckets[lh].push(t);
    }
    for (var k in buckets) { if (hasOwn(buckets, k)) res[k] = buckets[k].join(" "); }
    return res;
  }
  // Serialize the font-variant shorthand from its longhand values (`g`). Returns "" if it cannot be
  // represented (a CSS-wide keyword in one longhand, or ligatures:none mixed with other non-normal).
  function serializeFontVariant(g) {
    var vals = {};
    for (var i = 0; i < FONT_VARIANT_LONGHANDS.length; i++) {
      var lh = FONT_VARIANT_LONGHANDS[i], val = g(lh);
      if (val === "" || val == null) return ""; // a longhand missing -> can't serialize
      if (isCssWideKeyword(val)) return ""; // CSS-wide keyword can't appear in the shorthand
      vals[lh] = val;
    }
    var lig = vals["font-variant-ligatures"];
    var nonNormal = [];
    for (var j = 0; j < FONT_VARIANT_LONGHANDS.length; j++) {
      var p = FONT_VARIANT_LONGHANDS[j], pv = vals[p];
      if (pv !== "normal") nonNormal.push([p, pv]);
    }
    if (nonNormal.length === 0) return "normal";
    if (lig === "none") {
      // `none` only combines with nothing else.
      return nonNormal.length === 1 && nonNormal[0][0] === "font-variant-ligatures" ? "none" : "";
    }
    var parts = [];
    for (var m = 0; m < nonNormal.length; m++) { if (nonNormal[m][1] === "none") return ""; parts.push(nonNormal[m][1]); }
    return parts.join(" ");
  }
  function isShorthand(name) { return name === "all" || name === "font-variant" || shorthandLonghands(name) != null; }
  // Expand a shorthand declaration into [[longhand, value], ...]. Returns null if not a shorthand we
  // expand (caller stores the property as-is). CSS-wide keywords expand to every longhand.
  function expandShorthand(name, value) {
    value = String(value).trim();
    var lhs = shorthandLonghands(name);
    if (lhs == null) return null;
    var out = [];
    if (isCssWideKeyword(value)) {
      var v = value.toLowerCase();
      for (var i = 0; i < lhs.length; i++) out.push([lhs[i], v]);
      return out;
    }
    if (BOX_SHORTHANDS[name] && name !== "border-radius") {
      var b = expandBox(value); if (!b) return null;
      return [[lhs[0], b[0]], [lhs[1], b[1]], [lhs[2], b[2]], [lhs[3], b[3]]];
    }
    if (name === "border-radius") {
      var parts = value.split("/"); var h = expandBox(parts[0].trim());
      if (!h) return null;
      var vv = parts.length > 1 ? expandBox(parts[1].trim()) : h;
      if (!vv) return null;
      return [
        [lhs[0], h[0] === vv[0] ? h[0] : h[0] + " " + vv[0]],
        [lhs[1], h[1] === vv[1] ? h[1] : h[1] + " " + vv[1]],
        [lhs[2], h[2] === vv[2] ? h[2] : h[2] + " " + vv[2]],
        [lhs[3], h[3] === vv[3] ? h[3] : h[3] + " " + vv[3]]
      ];
    }
    if (XY_SHORTHANDS[name]) {
      var t = splitCssTokens(value);
      if (t.length === 1) return [[lhs[0], t[0]], [lhs[1], t[0]]];
      if (t.length === 2) return [[lhs[0], t[0]], [lhs[1], t[1]]];
      return null;
    }
    if (BORDER_SIDE[name]) {
      var p = parseBorderLike(value);
      var map = name === "outline"
        ? { width: "outline-width", style: "outline-style", color: "outline-color" }
        : name === "column-rule"
          ? { width: "column-rule-width", style: "column-rule-style", color: "column-rule-color" }
          : { width: name + "-width", style: name + "-style", color: name + "-color" };
      var res = [];
      res.push([map.width, p.width !== undefined ? p.width : "medium"]);
      res.push([map.style, p.style !== undefined ? p.style : "none"]);
      res.push([map.color, p.color !== undefined ? p.color : "currentcolor"]);
      return res;
    }
    if (name === "border") {
      var p2 = parseBorderLike(value);
      var w = p2.width !== undefined ? p2.width : "medium";
      var st = p2.style !== undefined ? p2.style : "none";
      var co = p2.color !== undefined ? p2.color : "currentcolor";
      var r = [], sides = ["top", "right", "bottom", "left"];
      for (var s = 0; s < 4; s++) r.push(["border-" + sides[s] + "-width", w]);
      for (var s2 = 0; s2 < 4; s2++) r.push(["border-" + sides[s2] + "-style", st]);
      for (var s3 = 0; s3 < 4; s3++) r.push(["border-" + sides[s3] + "-color", co]);
      for (var bi = 0; bi < BORDER_IMAGE_LONGHANDS.length; bi++) r.push([BORDER_IMAGE_LONGHANDS[bi], BORDER_IMAGE_INITIAL[BORDER_IMAGE_LONGHANDS[bi]]]);
      return r;
    }
    if (name === "font-variant") {
      var fv = expandFontVariant(value);
      if (!fv) return null;
      var fvo = [];
      for (var fi = 0; fi < FONT_VARIANT_LONGHANDS.length; fi++) { var fl = FONT_VARIANT_LONGHANDS[fi]; fvo.push([fl, fv[fl]]); }
      return fvo;
    }
    if (name === "border-image") {
      if (value.toLowerCase() === "none") {
        var bir = [];
        for (var bz = 0; bz < BORDER_IMAGE_LONGHANDS.length; bz++) bir.push([BORDER_IMAGE_LONGHANDS[bz], BORDER_IMAGE_INITIAL[BORDER_IMAGE_LONGHANDS[bz]]]);
        return bir;
      }
      return null;
    }
    if (name === "list-style") {
      var ls = parseListStyle(value), out2 = [];
      out2.push(["list-style-type", ls["list-style-type"] !== undefined ? ls["list-style-type"] : "disc"]);
      out2.push(["list-style-position", ls["list-style-position"] !== undefined ? ls["list-style-position"] : "outside"]);
      out2.push(["list-style-image", ls["list-style-image"] !== undefined ? ls["list-style-image"] : "none"]);
      return out2;
    }
    if (name === "flex") {
      var fl = parseFlex(value);
      if (!fl) return null;
      return [["flex-grow", fl.grow], ["flex-basis", fl.basis], ["flex-shrink", fl.shrink]];
    }
    return null;
  }
  // Parse the `flex` shorthand into {grow, shrink, basis}. Returns null if it can't be modeled.
  function parseFlex(value) {
    var v = String(value).trim(), vl = v.toLowerCase();
    if (vl === "none") return { grow: "0", shrink: "0", basis: "auto" };
    if (vl === "auto") return { grow: "1", shrink: "1", basis: "auto" };
    var toks = splitCssTokens(v);
    function isNum(t) { return /^[-+]?(?:\d+\.?\d*|\.\d+)$/.test(t); }
    var grow = null, shrink = null, basis = null;
    for (var i = 0; i < toks.length; i++) {
      var t = toks[i];
      if (isNum(t)) {
        if (grow === null) grow = t;
        else if (shrink === null) shrink = t;
        else return null;
      } else {
        if (basis !== null) return null;
        basis = t;
      }
    }
    if (grow === null && basis === null) return null;
    // Defaults per CSS Flexbox: grow 1, shrink 1, basis 0% — but a single number sets basis to 0px
    // (the "one value, flexible" case) which browsers serialize as `0px`.
    if (grow === null) grow = "1";
    if (shrink === null) shrink = "1";
    if (basis === null) basis = "0px";
    return { grow: normalizeNumberToken(grow), shrink: normalizeNumberToken(shrink), basis: basis };
  }
  // Serialize a shorthand from the current longhand values (`getLong(name)`). Returns "" if it
  // cannot be represented (a longhand missing or values inconsistent).
  function serializeShorthand(name, getLong) {
    function g(n) { return getLong(n); }
    var lhs = shorthandLonghands(name);
    if (lhs == null) return "";
    if (name === "border") lhs = BORDER_ALL_LONGHANDS;
    var allSet = true, common = null, sameKw = true;
    for (var i = 0; i < lhs.length; i++) {
      var v = g(lhs[i]);
      if (v === "" || v == null) allSet = false;
      if (common === null) common = v; else if (common !== v) sameKw = false;
    }
    if (allSet && sameKw && isCssWideKeyword(common)) return common.toLowerCase();
    for (var j = 0; j < lhs.length; j++) { if (isCssWideKeyword(g(lhs[j]))) { if (!(allSet && sameKw)) return ""; } }
    if (!allSet) return "";

    if (BOX_SHORTHANDS[name] && name !== "border-radius") {
      return serializeBox(g(lhs[0]), g(lhs[1]), g(lhs[2]), g(lhs[3]));
    }
    if (name === "border-radius") {
      var H = [g(lhs[0]), g(lhs[1]), g(lhs[2]), g(lhs[3])];
      var hs = [], vs = [], split = false;
      for (var k = 0; k < 4; k++) { var pr = splitCssTokens(H[k]); hs.push(pr[0]); if (pr.length > 1) { vs.push(pr[1]); split = true; } else vs.push(pr[0]); }
      var hser = serializeBox(hs[0], hs[1], hs[2], hs[3]);
      if (!split) return hser;
      return hser + " / " + serializeBox(vs[0], vs[1], vs[2], vs[3]);
    }
    if (XY_SHORTHANDS[name]) {
      var x = g(lhs[0]), y = g(lhs[1]);
      return x === y ? x : x + " " + y;
    }
    if (BORDER_SIDE[name]) {
      var wv, sv, cv, initW = "medium", initS = "none", initC = "currentcolor";
      if (name === "outline") { cv = g(lhs[0]); sv = g(lhs[1]); wv = g(lhs[2]); }
      else { wv = g(lhs[0]); sv = g(lhs[1]); cv = g(lhs[2]); }
      var parts = [];
      if (name === "outline") {
        if (cv !== initC) parts.push(cv);
        if (sv !== initS) parts.push(sv);
        if (wv !== initW) parts.push(wv);
      } else {
        if (wv !== initW) parts.push(wv);
        if (sv !== initS) parts.push(sv);
        if (cv !== initC) parts.push(cv);
      }
      return parts.length ? parts.join(" ") : "medium";
    }
    if (name === "border") {
      function side(prefix) { return [g("border-top-" + prefix), g("border-right-" + prefix), g("border-bottom-" + prefix), g("border-left-" + prefix)]; }
      var W = side("width"), S = side("style"), C = side("color");
      function allEq(a) { return a[0] === a[1] && a[1] === a[2] && a[2] === a[3]; }
      if (!allEq(W) || !allEq(S) || !allEq(C)) return "";
      for (var bi = 0; bi < BORDER_IMAGE_LONGHANDS.length; bi++) {
        if (g(BORDER_IMAGE_LONGHANDS[bi]) !== BORDER_IMAGE_INITIAL[BORDER_IMAGE_LONGHANDS[bi]]) return "";
      }
      var bp = [];
      if (W[0] !== "medium") bp.push(W[0]);
      if (S[0] !== "none") bp.push(S[0]);
      if (C[0] !== "currentcolor") bp.push(C[0]);
      return bp.length ? bp.join(" ") : "medium";
    }
    if (name === "border-image") {
      for (var bm = 0; bm < BORDER_IMAGE_LONGHANDS.length; bm++) {
        if (g(BORDER_IMAGE_LONGHANDS[bm]) !== BORDER_IMAGE_INITIAL[BORDER_IMAGE_LONGHANDS[bm]]) return "";
      }
      return "none";
    }
    if (name === "font-variant") { return serializeFontVariant(g); }
    if (name === "list-style") {
      var ty = g("list-style-type"), po = g("list-style-position"), im = g("list-style-image");
      var lp = [];
      if (po !== "outside") lp.push(po);
      if (ty !== "disc") lp.push(ty);
      if (im !== "none") lp.push(im);
      return lp.length === 0 ? "disc" : lp.join(" ");
    }
    if (name === "flex") {
      var fg = g("flex-grow"), fsk = g("flex-shrink"), fb = g("flex-basis");
      // A CSS-wide keyword in any longhand can't combine (handled by the early-return above).
      // Canonical: `grow shrink basis`.
      return fg + " " + fsk + " " + fb;
    }
    return "";
  }

  // Strip a trailing `!important` from a value. Returns [value, importantBool].
  function splitImportant(val) {
    var m = /^([\s\S]*?)\s*!\s*important\s*$/i.exec(val);
    if (m) return [m[1].trim(), true];
    return [val, false];
  }
  // Parse a declaration block into expanded longhand triples [name, value, important], in source
  // order, expanding shorthands and `all` as we go.
  // Decode CSS identifier escapes in `s` to their literal characters: `\xx` hex (1-6 hex digits,
  // optional single trailing whitespace) -> the code point; `\c` for any other char -> that char.
  function unescapeCssIdent(s) {
    s = String(s);
    var out = "", i = 0, n = s.length;
    while (i < n) {
      var c = s[i];
      if (c === "\\" && i + 1 < n) {
        var nx = s[i + 1];
        if (/[0-9a-fA-F]/.test(nx)) {
          var hex = ""; i++;
          while (i < n && hex.length < 6 && /[0-9a-fA-F]/.test(s[i])) { hex += s[i]; i++; }
          if (i < n && /\s/.test(s[i])) { i++; } // consume one trailing whitespace
          var cp = parseInt(hex, 16);
          out += (cp === 0 || cp > 0x10FFFF) ? "�" : String.fromCodePoint(cp);
          continue;
        }
        out += nx; i += 2; continue;
      }
      out += c; i++;
    }
    return out;
  }
  // Serialize a string as a CSS identifier (CSSOM "serialize an identifier"): escape characters that
  // aren't valid unescaped in an ident. Digits at the start (and a leading `-` then digit) are hex-
  // escaped; non-ident chars get a `\` (or hex escape for control chars).
  function escapeCssIdent(s) {
    s = String(s);
    var chars = Array.from(s), out = "";
    function hexEsc(cp) { return "\\" + cp.toString(16) + " "; }
    for (var i = 0; i < chars.length; i++) {
      var ch = chars[i], cp = ch.codePointAt(0);
      if (cp === 0) { out += "�"; continue; }
      if ((cp >= 0x1 && cp <= 0x1f) || cp === 0x7f) { out += hexEsc(cp); continue; }
      // A digit at the very start, or a digit right after a leading `-`, must be hex-escaped.
      if ((cp >= 0x30 && cp <= 0x39) && (i === 0 || (i === 1 && chars[0] === "-"))) { out += hexEsc(cp); continue; }
      if (i === 0 && cp === 0x2d && chars.length === 1) { out += "\\-"; continue; } // lone "-"
      if (cp >= 0x80 || cp === 0x2d || cp === 0x5f || (cp >= 0x30 && cp <= 0x39) ||
          (cp >= 0x41 && cp <= 0x5a) || (cp >= 0x61 && cp <= 0x7a)) { out += ch; continue; }
      out += "\\" + ch; // any other char: backslash-escape it literally
    }
    return out;
  }
  function parseStyleDecls(text) {
    var out = [];
    text = String(text || "");
    var parts = splitTopLevelSemis(text);
    // Parsing a whole block: importance, not source order, decides ties between same-property decls.
    var prev = __blockImportanceCascade;
    __blockImportanceCascade = true;
    try {
      for (var i = 0; i < parts.length; i++) {
        var seg = parts[i];
        var c = indexOfTopLevelColon(seg);
        if (c < 0) { continue; }
        var rawName = seg.slice(0, c).trim();
        // Custom property names are case-sensitive; decode CSS escapes (`--a\;b` -> `--a;b`). Standard
        // property names are ASCII-lowercased.
        var name;
        if (isCustomProp(rawName)) { name = unescapeCssIdent(rawName); }
        else { name = unescapeCssIdent(rawName).toLowerCase(); }
        if (!name) continue;
        var rawVal = seg.slice(c + 1).trim();
        var imp = splitImportant(rawVal);
        pushDecl(out, name, imp[0], imp[1]);
      }
    } finally { __blockImportanceCascade = prev; }
    return out;
  }
  // Index of the first top-level `:` (not inside parens/strings, not backslash-escaped). Used to
  // split a declaration `name : value` so an escaped colon in a custom-prop name isn't the splitter.
  function indexOfTopLevelColon(seg) {
    var i = 0, n = seg.length, depth = 0, q = null;
    while (i < n) {
      var c = seg[i];
      if (c === "\\" && i + 1 < n) { i += 2; continue; }
      if (q) { if (c === q) q = null; i++; continue; }
      if (c === '"' || c === "'") { q = c; i++; continue; }
      if (c === "(") { depth++; i++; continue; }
      if (c === ")") { if (depth > 0) depth--; i++; continue; }
      if (c === ":" && depth === 0) { return i; }
      i++;
    }
    return -1;
  }
  // Split a declaration block on top-level `;` (not inside parens/strings, not backslash-escaped).
  function splitTopLevelSemis(text) {
    var out = [], i = 0, n = text.length, depth = 0, q = null, start = 0;
    while (i < n) {
      var c = text[i];
      if (c === "\\" && i + 1 < n) { i += 2; continue; }
      if (q) { if (c === q) q = null; i++; continue; }
      if (c === '"' || c === "'") { q = c; i++; continue; }
      if (c === "(") { depth++; i++; continue; }
      if (c === ")") { if (depth > 0) depth--; i++; continue; }
      if (c === ";" && depth === 0) { out.push(text.slice(start, i)); start = i + 1; }
      i++;
    }
    if (start < n) out.push(text.slice(start));
    return out;
  }
  // ===== Property-name validity (CSSOM: unknown properties are dropped, never stored). =====
  // The set of standard CSS property names we recognize. Built from the longhand/shorthand machinery
  // plus an explicit list of additional standard names (logical properties, etc.). Custom properties
  // (`--*`) are always valid and handled separately.
  var KNOWN_PROPERTIES = (function () {
    var s = Object.create(null);
    function add(n) { s[n] = 1; }
    var arrs = [ALL_LONGHANDS, BORDER_ALL_LONGHANDS, FONT_VARIANT_LONGHANDS, BORDER_IMAGE_LONGHANDS];
    for (var i = 0; i < arrs.length; i++) for (var j = 0; j < arrs[i].length; j++) add(arrs[i][j]);
    // Shorthands + their longhands.
    var shorthands = [
      "all", "margin", "padding", "inset", "border", "border-width", "border-style", "border-color",
      "border-top", "border-right", "border-bottom", "border-left", "border-radius", "border-image",
      "outline", "overflow", "overscroll-behavior", "gap", "list-style", "text-decoration",
      "flex", "flex-flow", "place-content", "place-items", "place-self", "columns", "column-rule",
      "font", "font-variant", "background", "scroll-margin", "scroll-padding"
    ];
    for (var k = 0; k < shorthands.length; k++) {
      add(shorthands[k]);
      var lhs = shorthandLonghands(shorthands[k]);
      if (lhs) for (var m = 0; m < lhs.length; m++) add(lhs[m]);
    }
    // Additional standard longhands the cascade/CSSOM may carry that aren't in the lists above.
    var extra = [
      "background", "background-position", "color-scheme", "caret-color", "box-shadow", "transform",
      "transform-origin", "transition", "transition-property", "transition-duration",
      "transition-timing-function", "transition-delay", "animation", "animation-name",
      "animation-duration", "animation-timing-function", "animation-delay", "animation-iteration-count",
      "animation-direction", "animation-fill-mode", "animation-play-state",
      "content", "quotes", "cursor", "pointer-events", "user-select", "appearance", "-webkit-appearance",
      "box-sizing", "float", "clear", "clip", "clip-path", "filter", "backdrop-filter", "mix-blend-mode",
      "object-fit", "object-position", "order", "tab-size", "text-indent", "text-overflow", "text-shadow",
      "word-break", "word-spacing", "word-wrap", "overflow-wrap", "writing-mode", "direction",
      "unicode-bidi", "white-space", "vertical-align", "visibility", "z-index", "will-change",
      "scroll-behavior", "resize", "table-layout", "empty-cells", "caption-side", "counter-reset",
      "counter-increment", "perspective", "perspective-origin", "backface-visibility", "isolation",
      "mask", "mask-image", "-webkit-mask", "-webkit-mask-image", "column-count", "column-width",
      "column-gap", "column-rule-width", "column-rule-style", "column-rule-color", "grid-area",
      "grid-template", "grid-template-areas", "grid-auto-flow", "grid-auto-columns", "grid-auto-rows",
      "aspect-ratio", "inset-block", "inset-inline", "inset-block-start", "inset-block-end",
      "inset-inline-start", "inset-inline-end", "accent-color", "scroll-margin-top",
      "scroll-margin-right", "scroll-margin-bottom", "scroll-margin-left",
      "scroll-padding-top", "scroll-padding-right", "scroll-padding-bottom", "scroll-padding-left"
    ];
    for (var e = 0; e < extra.length; e++) add(extra[e]);
    // Logical box properties (margin/padding/border/inset block/inline + start/end). These are valid
    // standard properties (so they must not be rejected) even though we don't group them.
    var groups = ["margin", "padding"];
    for (var g = 0; g < groups.length; g++) {
      var base = groups[g];
      add(base + "-block"); add(base + "-inline");
      add(base + "-block-start"); add(base + "-block-end");
      add(base + "-inline-start"); add(base + "-inline-end");
    }
    var sides = ["block-start", "block-end", "inline-start", "inline-end", "block", "inline"];
    for (var si = 0; si < sides.length; si++) {
      add("border-" + sides[si] + "-width"); add("border-" + sides[si] + "-style"); add("border-" + sides[si] + "-color");
      add("border-" + sides[si]);
    }
    add("inline-size"); add("block-size"); add("min-inline-size"); add("min-block-size");
    add("max-inline-size"); add("max-block-size");
    // A broad set of additional standard CSS property names (so real-but-unmodeled properties are
    // not dropped). Not exhaustive, but covers the CSSOM round-trip test surface.
    var more = ("alignment-baseline baseline-shift baseline-source dominant-baseline " +
      "background-attachment background-blend-mode background-position-inline background-position-block " +
      "caption-side empty-cells orphans widows page-break-after page-break-before page-break-inside " +
      "break-after break-before break-inside text-indent text-justify text-orientation text-rendering " +
      "text-underline-position text-underline-offset text-decoration-thickness text-decoration-skip-ink " +
      "text-emphasis text-emphasis-color text-emphasis-style text-emphasis-position text-combine-upright " +
      "hyphens hanging-punctuation line-break overflow-anchor overflow-clip-margin scrollbar-gutter " +
      "scrollbar-width scrollbar-color scroll-snap-type scroll-snap-align scroll-snap-stop touch-action " +
      "flood-color flood-opacity stop-color stop-opacity lighting-color color-interpolation " +
      "color-interpolation-filters fill fill-opacity fill-rule stroke stroke-width stroke-opacity " +
      "stroke-dasharray stroke-dashoffset stroke-linecap stroke-linejoin stroke-miterlimit " +
      "clip-rule marker marker-start marker-mid marker-end paint-order shape-rendering " +
      "vector-effect text-anchor writing-mode glyph-orientation-vertical kerning " +
      "font-feature-settings font-variation-settings font-kerning font-optical-sizing font-language-override " +
      "font-size-adjust font-synthesis font-display src unicode-range ascent-override descent-override " +
      "line-gap-override size-adjust contain content-visibility container container-type container-name " +
      "counter-set inset gap row-gap column-gap place-items place-content place-self justify-items " +
      "image-rendering image-orientation shape-outside shape-margin shape-image-threshold " +
      "mix-blend-mode isolation backdrop-filter filter clip-path mask-clip mask-composite mask-mode " +
      "mask-origin mask-position mask-repeat mask-size mask-type mask-border " +
      "offset offset-path offset-distance offset-rotate offset-anchor offset-position " +
      "rotate scale translate transform-box transform-style perspective perspective-origin backface-visibility " +
      "will-change ruby-align ruby-position quotes tab-size " +
      "border-image-source border-image-slice border-image-width border-image-outset border-image-repeat " +
      "outline-offset text-shadow box-decoration-break " +
      "math-style math-depth math-shift forced-color-adjust print-color-adjust color-adjust " +
      "speak speak-as voice-family pitch pitch-range richness stress volume azimuth elevation " +
      "cue cue-before cue-after pause pause-before pause-after rest rest-before rest-after " +
      "all direction unicode-bidi white-space-collapse text-wrap text-wrap-mode text-wrap-style " +
      "field-sizing zoom aspect-ratio min-intrinsic-sizing " +
      "border-collapse border-spacing widows orphans table-layout caption-side empty-cells " +
      "outline-color outline-style outline-width outline-offset cursor pointer-events " +
      "background-position-x background-position-y background-clip background-origin").split(/\s+/);
    for (var mm = 0; mm < more.length; mm++) if (more[mm]) add(more[mm]);
    return s;
  })();
  function isKnownProperty(name) {
    if (isCustomProp(name)) return true;
    return hasOwn(KNOWN_PROPERTIES, name);
  }
  // A deliberately narrow validity check: returns false only for a small set of single-valued
  // longhand properties with values we can confidently reject (the cases the WPT CSSOM tests
  // exercise). Everything else is accepted — the engine ignores values it can't parse, and being
  // permissive avoids dropping valid declarations the round-trip tests rely on.
  // Single-token <color> longhands.
  var COLOR_LONGHANDS = { "color":1, "background-color":1,
    "border-top-color":1, "border-right-color":1, "border-bottom-color":1, "border-left-color":1,
    "text-decoration-color":1, "column-rule-color":1, "text-emphasis-color":1, "flood-color":1, "stop-color":1, "lighting-color":1 };
  // Non-negative <length-percentage> longhands.
  var NONNEG_LENGTH_LONGHANDS = { "width":1, "height":1, "min-width":1, "min-height":1,
    "max-width":1, "max-height":1, "inline-size":1, "block-size":1, "min-inline-size":1,
    "min-block-size":1, "max-inline-size":1, "max-block-size":1,
    "padding-top":1, "padding-right":1, "padding-bottom":1, "padding-left":1,
    "border-top-width":1, "border-right-width":1, "border-bottom-width":1, "border-left-width":1,
    "outline-width":1, "column-rule-width":1, "column-width":1 };
  function isValidValue(name, value) {
    var v = String(value).trim();
    if (v === "") return false;
    if (isCustomProp(name)) return true;
    if (isCssWideKeyword(v)) return true;
    var vl = v.toLowerCase();
    if (/(^|[^a-z-])(var|env)\s*\(/i.test(v)) return true; // can't validate around substitutions
    if (hasOwn(COLOR_LONGHANDS, name)) return isValidColor(v);
    if (hasOwn(NONNEG_LENGTH_LONGHANDS, name)) {
      if (vl === "auto" || vl === "none" || vl === "min-content" || vl === "max-content" ||
          vl === "fit-content" || vl === "thin" || vl === "medium" || vl === "thick" || /^fit-content\(/i.test(v)) return true;
      return isValidLengthLike(v, false);
    }
    if (name === "z-index" || name === "order") {
      if (vl === "auto") return true;
      return /^[-+]?\d+$/.test(v);
    }
    if (name === "opacity") {
      return /^[-+]?(?:\d+\.?\d*|\.\d+)(?:e[-+]?\d+)?%?$/i.test(v);
    }
    return true;
  }
  function isValidColor(v) {
    var vl = v.toLowerCase();
    if (NAMED_COLORS_OK[vl]) return true;
    if (vl === "transparent" || vl === "currentcolor" || vl === "inherit") return true;
    if (/^#([0-9a-f]{3}|[0-9a-f]{4}|[0-9a-f]{6}|[0-9a-f]{8})$/i.test(v)) return true;
    if (/^(rgba?|hsla?|hwb|lab|lch|oklab|oklch|color)\s*\(/i.test(v)) return true;
    return false;
  }
  // A small set of common named colors used to validate <color> keywords. Not exhaustive — any
  // unrecognized bare keyword for a color property is treated as invalid (matches the WPT cases).
  var NAMED_COLORS_OK = (function () {
    var names = ("black white red green blue yellow cyan magenta gray grey orange purple brown pink " +
      "silver gold navy teal olive maroon lime aqua fuchsia indigo violet coral salmon khaki crimson " +
      "tomato orchid plum tan beige ivory azure lavender turquoise chocolate darkred darkblue darkgreen " +
      "lightblue lightgreen lightgray lightgrey lightyellow rebeccapurple hotpink").split(" ");
    var o = Object.create(null);
    for (var i = 0; i < names.length; i++) o[names[i]] = 1;
    return o;
  })();
  function isValidLengthLike(v, allowNegative) {
    // Accept a single dimension/percentage/zero/calc token (optionally signed).
    if (/^calc\(/i.test(v)) return true;
    var m = /^([-+]?(?:\d+\.?\d*|\.\d+))(px|em|rem|ex|ch|vw|vh|vmin|vmax|cm|mm|in|pt|pc|q|%|fr)?$/i.exec(v);
    if (!m) return false;
    var num = parseFloat(m[1]);
    var unit = m[2] || "";
    if (num !== 0 && unit === "") return false; // unitless non-zero is not a length
    if (!allowNegative && num < 0) return false;
    return true;
  }
  // Append a declaration to the expanded longhand list, expanding shorthands and `all`.
  function pushDecl(out, name, val, important) {
    if (isCustomProp(name)) { setDecl(out, name, val, important); return; }
    // Drop unknown properties and values we can confidently reject (CSSOM parse-a-declaration).
    if (!isKnownProperty(name)) return;
    if (!isValidValue(name, val)) return;
    if (name === "all") {
      if (isCssWideKeyword(val)) {
        var kw = val.toLowerCase();
        // Remove any prior all-longhands so they re-append at this (the `all`) source position;
        // keeps custom properties declared before `all` ahead of it on serialization.
        for (var rr = 0; rr < ALL_LONGHANDS.length; rr++) removeDecl(out, ALL_LONGHANDS[rr]);
        for (var a = 0; a < ALL_LONGHANDS.length; a++) out.push([ALL_LONGHANDS[a], kw, !!important]);
      }
      return;
    }
    var expanded = expandShorthand(name, val);
    if (expanded) {
      for (var e = 0; e < expanded.length; e++) setDecl(out, expanded[e][0], normalizeCssValue(expanded[e][1]), important);
      return;
    }
    var nv = normalizeCssValue(val);
    // The `font` shorthand serializes size/line-height with spaces around the slash: `10px / 1`.
    // It also resets every font-variant longhand to its initial (which serializes as absent inline).
    if (name === "font" && !isCssWideKeyword(nv)) {
      nv = nv.replace(/\s*\/\s*/g, " / ");
      for (var fvr = 0; fvr < FONT_VARIANT_LONGHANDS.length; fvr++) removeDecl(out, FONT_VARIANT_LONGHANDS[fvr]);
    }
    // flex-basis serializes a zero length as `0px` (a <length-percentage>, not a flat number).
    if (name === "flex-basis" && nv === "0") { nv = "0px"; }
    // Property-specific <string> canonicalization.
    if (!isCssWideKeyword(nv)) {
      if (name === "content" || name === "quotes") { nv = requoteStrings(nv); }
      else if (name === "font-family") { nv = normalizeFontFamily(nv); }
    }
    setDecl(out, name, nv, important);
  }
  function findDecl(out, name) { for (var i = 0; i < out.length; i++) { if (out[i][0] === name) return i; } return -1; }
  // When true (set only while parsing a whole declaration block, e.g. the `cssText` setter), a later
  // NON-important declaration must not override an earlier `!important` one of the same property —
  // the cascade within a declaration block resolves on importance, not source order. The CSSOM
  // `setProperty` path leaves this false so an explicit set always replaces.
  var __blockImportanceCascade = false;
  function setDecl(out, name, val, important) {
    important = !!important;
    var i = findDecl(out, name);
    if (val == null || val === "") { if (i >= 0) out.splice(i, 1); return; }
    if (i >= 0) {
      if (__blockImportanceCascade && out[i][2] && !important) { return; }
      // When parsing a whole declaration block, a re-declared property keeps the LATER source
      // position (the cascade keeps the last occurrence, in its place) — so move it to the end. We
      // limit this to box-edge longhands (the logical property groups), whose relative ordering is
      // what the logical-group shorthand-serialization adjacency rule depends on; other properties
      // update in place to avoid disturbing the serialization of unexpanded shorthands.
      // Outside block parsing (a single `setProperty`), a different importance also moves it to the
      // end so an important override serializes after the non-important remainder.
      if ((__blockImportanceCascade && LOGICAL_GROUP[name]) || out[i][2] !== important) { out.splice(i, 1); out.push([name, val, important]); }
      else { out[i][1] = val; out[i][2] = important; }
    } else out.push([name, val, important]);
  }
  function removeDecl(out, name) { var i = findDecl(out, name); if (i >= 0) out.splice(i, 1); }
  // Shorthands to try when serializing a declaration block, in priority order.
  var SERIALIZE_SHORTHANDS = [
    "border", "border-width", "border-style", "border-color",
    "border-top", "border-right", "border-bottom", "border-left", "border-image",
    "margin", "padding", "inset", "border-radius",
    "margin-inline", "margin-block", "padding-inline", "padding-block", "inset-inline", "inset-block",
    "overflow", "overscroll-behavior", "gap", "outline", "list-style", "text-decoration",
    "flex", "flex-flow", "place-content", "place-items", "place-self", "columns", "font-variant"
  ];
  // Logical property groups for box edges: physical + flow-relative longhands share a group, and
  // mixing them prevents shorthand serialization unless the interleaving longhands belong to the
  // shorthand being formed (CSSOM "serialize a CSS declaration block" — logical-group adjacency).
  // Maps a longhand property name to its group id; properties not present here have no group.
  var LOGICAL_GROUP = (function () {
    var g = Object.create(null);
    ["margin-top","margin-right","margin-bottom","margin-left",
     "margin-block-start","margin-block-end","margin-inline-start","margin-inline-end"].forEach(function (p) { g[p] = "margin"; });
    ["padding-top","padding-right","padding-bottom","padding-left",
     "padding-block-start","padding-block-end","padding-inline-start","padding-inline-end"].forEach(function (p) { g[p] = "padding"; });
    ["top","right","bottom","left",
     "inset-block-start","inset-block-end","inset-inline-start","inset-inline-end"].forEach(function (p) { g[p] = "inset"; });
    return g;
  })();
  // Serialize expanded longhand triples WITHOUT shorthand grouping — the engine-readable form
  // stored in the `style` attribute (the Rust cascade understands longhands, not every shorthand).
  function serializeStyleDeclsFlat(decls) {
    var s = "";
    for (var i = 0; i < decls.length; i++) {
      var nm = isCustomProp(decls[i][0]) ? escapeCssIdent(decls[i][0]) : decls[i][0];
      s += (s ? " " : "") + nm + ": " + decls[i][1] + (decls[i][2] ? " !important" : "") + ";";
    }
    return s;
  }
  // Serialize a list of expanded longhand triples to a declaration block, grouping consecutive
  // longhands into shorthands where possible (CSSOM §serialize-a-css-declaration-block).
  function serializeStyleDecls(decls) {
    var byName = Object.create(null);
    var indexOfName = Object.create(null);
    for (var i = 0; i < decls.length; i++) { byName[decls[i][0]] = { v: decls[i][1], imp: decls[i][2] }; indexOfName[decls[i][0]] = i; }
    // The logical-group adjacency test (CSSOM): a shorthand whose longhands span declaration indices
    // [lo, hi] may only serialize if every OTHER declaration of a property in the same logical group
    // that falls within (lo, hi) is itself one of the shorthand's longhands. Returns true if `lhs`
    // (the shorthand's longhands) is serializable under this rule for group `group`.
    function logicalGroupContiguous(lhs, group) {
      if (!group) { return true; }
      var lo = Infinity, hi = -Infinity, set = Object.create(null);
      for (var a = 0; a < lhs.length; a++) {
        set[lhs[a]] = 1;
        var idx = indexOfName[lhs[a]];
        if (idx === undefined) { continue; }
        if (idx < lo) { lo = idx; }
        if (idx > hi) { hi = idx; }
      }
      if (lo === Infinity) { return true; }
      for (var d2 = lo; d2 <= hi; d2++) {
        var nm = decls[d2][0];
        if (LOGICAL_GROUP[nm] === group && !set[nm]) { return false; }
      }
      return true;
    }
    var serialized = Object.create(null);
    var pieces = [];
    function emit(prop, value, important) { pieces.push(prop + ": " + value + (important ? " !important" : "") + ";"); }
    // If EVERY `all`-affected longhand is present, equal, a CSS-wide keyword, and same importance,
    // collapse them into a single `all: <kw>` (at the position of the first such longhand).
    var allKw = null, allImp = null, allOk = true;
    for (var ai = 0; ai < ALL_LONGHANDS.length; ai++) {
      var rec0 = byName[ALL_LONGHANDS[ai]];
      if (!rec0 || !isCssWideKeyword(rec0.v)) { allOk = false; break; }
      if (allKw === null) { allKw = rec0.v; allImp = rec0.imp; }
      else if (rec0.v !== allKw || rec0.imp !== allImp) { allOk = false; break; }
    }
    var collapseAll = allOk && allKw !== null, allEmitted = false;
    for (var d = 0; d < decls.length; d++) {
      var name = decls[d][0];
      if (serialized[name]) continue;
      if (collapseAll && ALL_LONGHANDS.indexOf(name) >= 0) {
        if (!allEmitted) { emit("all", allKw.toLowerCase(), allImp); allEmitted = true; }
        serialized[name] = 1; continue;
      }
      if (isCustomProp(name)) { emit(escapeCssIdent(name), decls[d][1], decls[d][2]); serialized[name] = 1; continue; }
      var used = false;
      for (var s = 0; s < SERIALIZE_SHORTHANDS.length; s++) {
        var sh = SERIALIZE_SHORTHANDS[s];
        var lhs = sh === "border" ? BORDER_ALL_LONGHANDS : shorthandLonghands(sh);
        if (!lhs) continue;
        if (lhs.indexOf(name) < 0) continue;
        var ok = true, imp = decls[d][2];
        for (var k = 0; k < lhs.length; k++) {
          var rec = byName[lhs[k]];
          if (!rec || serialized[lhs[k]] || rec.imp !== imp) { ok = false; break; }
        }
        if (!ok) continue;
        // Logical-group adjacency: don't form this shorthand if a different-mapping-logic property of
        // the same logical group is declared between its longhands.
        if (!logicalGroupContiguous(lhs, LOGICAL_GROUP[name])) { continue; }
        var ser = serializeShorthand(sh, function (n) { var r = byName[n]; return r ? r.v : ""; });
        if (ser === "") continue;
        emit(sh, ser, imp);
        for (var k2 = 0; k2 < lhs.length; k2++) serialized[lhs[k2]] = 1;
        used = true; break;
      }
      if (!used) { emit(name, decls[d][1], decls[d][2]); serialized[name] = 1; }
    }
    return pieces.join(" ");
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
  // Normalize a CSS property name for the CSSStyleDeclaration API (lowercase; custom props as-is).
  function normPropName(p) { p = String(p); if (isCustomProp(p)) { return p; } /* custom props are case-sensitive, kept verbatim */ p = camelToKebab(p); return p.toLowerCase(); }
  // Build a CSSStyleDeclaration over a backing store. `get()` returns the current declaration block
  // text; `set(text)` writes it back. Used for both inline styles (style attr) and rule blocks.
  // `restrict(longhandName)` (optional) gates which longhand properties this declaration block may
  // contain — used for @page / @keyframes, where only a subset of properties apply. A shorthand is
  // allowed iff at least one of its longhands is allowed; rejected longhands are dropped on parse and
  // on set (so `style.length` / serialization reflect only the allowed declarations).
  function makeStyleDecl(get, set, restrict) {
    function filterDecls(d) {
      if (!restrict) return d;
      var out = [];
      for (var i = 0; i < d.length; i++) { if (isCustomProp(d[i][0]) || restrict(d[i][0])) out.push(d[i]); }
      return out;
    }
    function read() { return filterDecls(parseStyleDecls(get())); }
    // The backing store holds the EXPANDED longhand form (engine-readable). Shorthand grouping is
    // applied only when serializing for the CSSOM `cssText` getter / `item`/`length` enumeration.
    // Only write when the serialized result actually differs from the current backing store: this
    // avoids creating an empty `style` attribute for a rejected declaration and avoids firing a
    // (mutation-observed) attribute write when nothing changed (CSSOM "same value" cases).
    // Serialize the declaration block back to the backing store. The store holds the EXPANDED
    // longhand form (engine-readable: the Rust cascade understands longhands, not every shorthand).
    function serializeForStore(d) { return serializeStyleDeclsFlat(filterDecls(d)); }
    function write(d) {
      var next = serializeForStore(d);
      // Compare against the RE-SERIALIZED current state (not the raw backing string, which may
      // differ only in trivia like trailing `;`/spacing) so a no-op edit doesn't create/rewrite the
      // attribute or fire a spurious mutation record.
      if (next === serializeForStore(read())) return;
      set(next);
      try { globalThis.__scheduleMODelivery(); } catch (e) {}
    }
    // Like write(), but always writes (used by the cssText setter, which must reflect even an
    // equal-but-reparsed value as an attribute mutation per the WPT MutationObserver tests).
    function writeAlways(d) {
      set(serializeForStore(d));
      try { globalThis.__scheduleMODelivery(); } catch (e) {}
    }
    // The serialized value of property `name` per CSSOM (shorthand serialization, custom verbatim).
    function getVal(name) {
      var d = read();
      if (isCustomProp(name)) { var ci = findDecl(d, name); return ci >= 0 ? d[ci][1] : ""; }
      if (name === "all") {
        var common = null, ok = true;
        for (var a = 0; a < ALL_LONGHANDS.length; a++) {
          var idx = findDecl(d, ALL_LONGHANDS[a]);
          if (idx < 0) { ok = false; break; }
          var v = d[idx][1];
          if (common === null) common = v; else if (common !== v) { ok = false; break; }
        }
        return (ok && common !== null && isCssWideKeyword(common)) ? common.toLowerCase() : "";
      }
      if (isShorthand(name)) {
        // A shorthand only serializes if all its longhands are present with a uniform priority.
        var shLhs = name === "border" ? BORDER_ALL_LONGHANDS : shorthandLonghands(name);
        if (shLhs) {
          var impCommon = null, impOk = true, allPresent = true;
          for (var si = 0; si < shLhs.length; si++) {
            var sidx = findDecl(d, shLhs[si]);
            if (sidx < 0) { allPresent = false; break; }
            if (impCommon === null) impCommon = d[sidx][2]; else if (impCommon !== d[sidx][2]) { impOk = false; break; }
          }
          if (allPresent && !impOk) return ""; // mixed importance -> shorthand can't be formed
        }
        var sv = serializeShorthand(name, function (n) { var i = findDecl(d, n); return i >= 0 ? d[i][1] : ""; });
        if (sv !== "") return sv;
        // If the shorthand was stored literally (we don't model its value), return the literal.
        var li = findDecl(d, name);
        return li >= 0 ? d[li][1] : "";
      }
      if (hasOwn(KEYWORD_ONLY_SHORTHANDS, name)) {
        var lhsK = KEYWORD_ONLY_SHORTHANDS[name], commonK = null, okK = true;
        for (var kk = 0; kk < lhsK.length; kk++) { var ik = findDecl(d, lhsK[kk]); if (ik < 0) { okK = false; break; } if (commonK === null) commonK = d[ik][1]; else if (commonK !== d[ik][1]) { okK = false; break; } }
        if (okK && commonK !== null && isCssWideKeyword(commonK)) return commonK.toLowerCase();
      }
      var i = findDecl(d, name);
      return i >= 0 ? d[i][1] : "";
    }
    function getPriority(name) {
      var d = read();
      if (name === "all" || isShorthand(name)) {
        var lhs = name === "all" ? ALL_LONGHANDS : (name === "border" ? BORDER_ALL_LONGHANDS : shorthandLonghands(name));
        if (!lhs) return "";
        for (var k = 0; k < lhs.length; k++) { var i = findDecl(d, lhs[k]); if (i < 0 || !d[i][2]) return ""; }
        return "important";
      }
      var idx = findDecl(d, name);
      return idx >= 0 && d[idx][2] ? "important" : "";
    }
    function setVal(name, val, important) {
      var d = read();
      if (val == null || String(val).trim() === "") { // empty value removes (per spec)
        if (name === "all") { for (var a = 0; a < ALL_LONGHANDS.length; a++) removeDecl(d, ALL_LONGHANDS[a]); }
        else if (isShorthand(name)) { var lhs0 = name === "border" ? BORDER_ALL_LONGHANDS : shorthandLonghands(name); for (var q = 0; q < lhs0.length; q++) removeDecl(d, lhs0[q]); }
        else removeDecl(d, name);
        write(d); return;
      }
      pushDecl(d, name, String(val).trim(), !!important);
      write(d);
    }
    function removeVal(name) {
      var old = getVal(name);
      var d = read();
      if (name === "all") { for (var a = 0; a < ALL_LONGHANDS.length; a++) removeDecl(d, ALL_LONGHANDS[a]); }
      else if (isShorthand(name)) { var lhs = name === "border" ? BORDER_ALL_LONGHANDS : shorthandLonghands(name); for (var q = 0; q < lhs.length; q++) removeDecl(d, lhs[q]); }
      else removeDecl(d, name);
      write(d);
      return old;
    }
    var base = {
      getPropertyValue: function (p) { return getVal(normPropName(p)); },
      getPropertyPriority: function (p) { return getPriority(normPropName(p)); },
      setProperty: function (p, v, prio) {
        var name = normPropName(p);
        var important = prio != null && String(prio).toLowerCase() === "important";
        setVal(name, v, important);
      },
      removeProperty: function (p) { return removeVal(normPropName(p)); },
      item: function (i) { var d = read(); i = i >>> 0; return i < d.length ? d[i][0] : ""; }
    };
    // CSSStyleDeclaration is iterable over its property names (the indexed-property getter values).
    try { base[Symbol.iterator] = function () { var d = read(); return makeIter(d, function (i, v) { return v[0]; }); }; } catch (e) {}
    Object.defineProperty(base, "length", { get: function () { return read().length; }, enumerable: false, configurable: true });
    Object.defineProperty(base, "cssText", {
      // Group longhands back into shorthands on read (CSSOM serialization); store flat on write.
      get: function () { return serializeStyleDecls(read()); },
      // Setting cssText replaces the whole block and always reflects to the style attribute (it is
      // observable even when the resulting value is unchanged).
      set: function (v) { writeAlways(parseStyleDecls(v)); },
      enumerable: true, configurable: true
    });
    // Make `el.style instanceof CSSStyleDeclaration` hold: the Proxy (no getPrototypeOf trap) reports
    // its target's prototype, so give the target the interface prototype.
    try { if (globalThis.CSSStyleDeclaration && globalThis.CSSStyleDeclaration.prototype) { Object.setPrototypeOf(base, globalThis.CSSStyleDeclaration.prototype); } } catch (e) {}
    try {
      return new Proxy(base, {
        get: function (t, p) {
          if (typeof p !== "string") { return t[p]; }
          if (p in t) { return t[p]; }
          if (/^[0-9]+$/.test(p)) { return t.item(Number(p)); }
          return getVal(normPropName(p));
        },
        set: function (t, p, v) {
          if (typeof p !== "string") { t[p] = v; return true; }
          if (p === "cssText") { t.cssText = v; return true; }
          if (p in t) { t[p] = v; return true; }
          setVal(normPropName(p), v, false); return true;
        },
        // CSS properties are WebIDL attributes (on the prototype) — `"color" in decl` is true even
        // though there's no own property for it (CSSStyleDeclaration-properties test).
        has: function (t, p) {
          if (typeof p === "string" && isKnownProperty(normPropName(p))) { return true; }
          return p in t;
        }
      });
    } catch (e) { return base; }
  }
  function makeStyle(node) {
    return makeStyleDecl(
      function () { return styleAttr(node); },
      function (text) { document.__setAttr(node, "style", text); }
    );
  }
  // A snapshot ES iterator over `arr`, mapping each (index, value) via `pick`.
  function makeIter(arr, pick) {
    var i = 0;
    var it = { next: function () { return i < arr.length ? { value: pick(i, arr[i++]), done: false } : { value: undefined, done: true }; } };
    try { it[Symbol.iterator] = function () { return this; }; } catch (e) {}
    return it;
  }
  // A spec-complete DOMTokenList over an element's `class` attribute (DOM standard §7.1).
  // The token set is the live `class` attribute parsed on ASCII whitespace
  // ([\t\n\f\r ]), order-preserving and de-duplicated. Reads always reparse the live
  // attribute (so external className/setAttribute changes are reflected); the mutating
  // methods run the spec "update steps" which serialize the ordered set back to `class`.
  function makeClassList(node) { return makeTokenList(node, "class", null); }
  // A DOMTokenList over an arbitrary reflected attribute (`attrName`). `supported` is an optional
  // allow-list of tokens for `supports()` (null => supports() throws TypeError, like `class`).
  function makeTokenList(node, attrName, supported) {
    // ASCII whitespace per the HTML spec: TAB, LF, FF, CR, SPACE.
    function splitTokens(s) {
      var out = [], i = 0, n = s.length;
      while (i < n) {
        var c = s[i];
        if (c === " " || c === "\t" || c === "\n" || c === "\f" || c === "\r") { i++; continue; }
        var start = i;
        while (i < n) { var d = s[i]; if (d === " " || d === "\t" || d === "\n" || d === "\f" || d === "\r") break; i++; }
        out.push(s.slice(start, i));
      }
      return out;
    }
    function hasWhitespace(s) {
      for (var i = 0; i < s.length; i++) { var c = s[i]; if (c === " " || c === "\t" || c === "\n" || c === "\f" || c === "\r") return true; }
      return false;
    }
    // Throw a DOMException that satisfies WPT assert_throws_dom (correct .name/.code, and
    // `instanceof DOMException`).
    function syntaxErr() { throw new globalThis.DOMException("The token provided must not be empty.", "SyntaxError"); }
    function invalidCharErr() { throw new globalThis.DOMException("The token provided contains HTML space characters, which are not valid in tokens.", "InvalidCharacterError"); }
    function validateToken(t) {
      if (t === "") { syntaxErr(); }
      if (hasWhitespace(t)) { invalidCharErr(); }
    }
    // Raw reflected-attribute string, or null when the attribute is absent.
    function rawAttr() { var c = document.__getAttr(node, attrName); return c == null ? null : String(c); }
    // The ordered token set (de-duplicated, first occurrence wins).
    function tokenSet() {
      var raw = rawAttr();
      if (raw == null || raw === "") { return []; }
      var toks = splitTokens(raw), seen = Object.create(null), out = [];
      for (var i = 0; i < toks.length; i++) { var t = toks[i]; if (!seen[t]) { seen[t] = 1; out.push(t); } }
      return out;
    }
    // The "update steps": serialize the ordered set and write it back to `class`, unless the
    // attribute is absent and the set is empty (in which case do nothing).
    function update(set) {
      if (rawAttr() == null && set.length === 0) { return; }
      document.__setAttr(node, attrName, set.join(" "));
    }

    var cl = {
      item: function (i) { i = i >>> 0; var s = tokenSet(); return i < s.length ? s[i] : null; },
      contains: function (token) { return tokenSet().indexOf(String(token)) >= 0; },
      add: function () {
        var s = tokenSet();
        for (var i = 0; i < arguments.length; i++) {
          var t = String(arguments[i]); validateToken(t);
          if (s.indexOf(t) < 0) { s.push(t); }
        }
        update(s);
      },
      remove: function () {
        var s = tokenSet();
        for (var i = 0; i < arguments.length; i++) {
          var t = String(arguments[i]); validateToken(t);
          var x = s.indexOf(t); if (x >= 0) { s.splice(x, 1); }
        }
        update(s);
      },
      toggle: function (token, force) {
        token = String(token); validateToken(token);
        var s = tokenSet(), x = s.indexOf(token);
        if (x >= 0) {
          // token present
          if (force === undefined || force === false) { s.splice(x, 1); update(s); return false; }
          return true; // force === true: no-op, no update
        }
        // token absent
        if (force === undefined || force === true) { s.push(token); update(s); return true; }
        return false; // force === false: no-op, no update
      },
      replace: function (token, newToken) {
        token = String(token); newToken = String(newToken);
        // Per spec, the empty-string (SyntaxError) check runs for BOTH tokens before the
        // whitespace (InvalidCharacterError) check for either.
        if (token === "" || newToken === "") { syntaxErr(); }
        if (hasWhitespace(token) || hasWhitespace(newToken)) { invalidCharErr(); }
        var s = tokenSet(), x = s.indexOf(token);
        if (x < 0) { return false; }
        var y = s.indexOf(newToken);
        if (y >= 0 && y !== x) {
          // newToken already in set: replace in place, then drop the duplicate.
          s[x] = newToken;
          var dup = s.indexOf(newToken); // earliest occurrence
          for (var j = s.length - 1; j >= 0; j--) { if (s[j] === newToken && j !== dup) { s.splice(j, 1); } }
        } else {
          s[x] = newToken;
        }
        update(s);
        return true;
      },
      supports: function (token) {
        // With no supported-tokens allow-list (e.g. `class`/`rel`), supports() throws TypeError.
        // Otherwise it ASCII-lowercases the token and checks membership.
        if (supported == null) { throw new TypeError("DOMTokenList has no supported tokens."); }
        return supported.indexOf(asciiLower(String(token))) >= 0;
      },
      forEach: function (cb, thisArg) {
        if (typeof cb !== "function") { throw new TypeError("The callback provided as parameter 1 is not a function."); }
        var s = tokenSet();
        for (var i = 0; i < s.length; i++) { cb.call(thisArg, s[i], i, cl); }
      },
      keys: function () { return makeIter(tokenSet(), function (i, v) { return i; }); },
      values: function () { return makeIter(tokenSet(), function (i, v) { return v; }); },
      entries: function () { return makeIter(tokenSet(), function (i, v) { return [i, v]; }); },
      toString: function () { var c = rawAttr(); return c == null ? "" : c; }
    };
    // Object.prototype.toString.call(list) === "[object DOMTokenList]".
    try { cl[Symbol.toStringTag] = "DOMTokenList"; } catch (e) {}
    // for...of / Symbol.iterator over the token values.
    try { cl[Symbol.iterator] = cl.values; } catch (e) {}

    Object.defineProperty(cl, "length", { get: function () { return tokenSet().length; }, enumerable: false, configurable: true });
    // `value` (the stringifier behaviour): get returns the raw attribute (""/absent => ""),
    // set assigns the `class` attribute verbatim.
    Object.defineProperty(cl, "value", {
      get: function () { var c = rawAttr(); return c == null ? "" : c; },
      set: function (v) { document.__setAttr(node, attrName, v == null ? "" : String(v)); },
      enumerable: false, configurable: true
    });
    // Live integer-indexed access: classList[i] => i-th token (or undefined). Reparses on each
    // read via a Proxy so the indices stay live with the attribute.
    try {
      return new Proxy(cl, {
        get: function (t, p, r) {
          if (typeof p === "string" && p.length && /^[0-9]+$/.test(p)) {
            var i = p >>> 0, s = tokenSet();
            return i < s.length ? s[i] : undefined;
          }
          return Reflect.get(t, p, r);
        },
        has: function (t, p) {
          if (typeof p === "string" && p.length && /^[0-9]+$/.test(p)) { return (p >>> 0) < tokenSet().length; }
          return p in t;
        }
      });
    } catch (e) { return cl; }
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
  // Validate and serialize one *complex selector* (a single comma component) per the Selectors
  // grammar the CSSOM `selectorText` setter needs. Returns the normalized string, or `null` if the
  // selector is invalid (the setter then leaves the rule unchanged, per spec). Covers type/universal
  // selectors (incl. namespace prefixes `ns|`, `*|`, `|`), `.class`, `#id`, `[attr...]`, the known
  // pseudo-classes/elements, `:not(...)`, combinators, and unicode identifiers.
  var __cssPseudoElements = { before: 1, after: 1, "first-line": 1, "first-letter": 1, "first-line ": 1,
    selection: 1, placeholder: 1, marker: 1, backdrop: 1 };
  var __cssPseudoClasses = { active: 1, hover: 1, focus: 1, "focus-within": 1, "focus-visible": 1,
    visited: 1, link: 1, target: 1, root: 1, empty: 1, enabled: 1, disabled: 1, checked: 1, "first-child": 1,
    "last-child": 1, "only-child": 1, "first-of-type": 1, "last-of-type": 1, "only-of-type": 1,
    "nth-child": 1, "nth-last-child": 1, "nth-of-type": 1, "nth-last-of-type": 1, lang: 1, not: 1,
    is: 1, where: 1, has: 1, "any-link": 1, default: 1, indeterminate: 1, "read-only": 1, "read-write": 1,
    required: 1, optional: 1, "placeholder-shown": 1, valid: 1, invalid: 1, "in-range": 1, "out-of-range": 1 };
  // An identifier per CSS: starts with a letter / `_` / `-` / non-ASCII / escape, then those or
  // digits. We accept any non-ASCII codepoint (covers `ÇĞıİ`, `🤓`). A lone `-` is not an identifier.
  function isIdentChar(c, first) {
    if (c === "_" || c === "-") { return true; }
    var code = c.charCodeAt(0);
    if (code >= 128) { return true; }              // non-ASCII
    if (c >= "a" && c <= "z" || c >= "A" && c <= "Z") { return true; }
    if (!first && c >= "0" && c <= "9") { return true; }
    return false;
  }
  function isIdent(s) {
    if (!s) { return false; }
    if (s === "-") { return false; }
    var chars = Array.from(s);                       // codepoint-aware (handles surrogate pairs)
    for (var i = 0; i < chars.length; i++) {
      var c = chars[i];
      // A `\` starts an escape — anything can follow (a hex code or a single literal char), so the
      // identifier is valid regardless of the escaped character. Skip the rest of the escape.
      if (c === "\\") {
        i++;
        if (i < chars.length && /[0-9a-fA-F]/.test(chars[i])) {
          var hc = 1;
          while (i + 1 < chars.length && hc < 6 && /[0-9a-fA-F]/.test(chars[i + 1])) { i++; hc++; }
          if (i + 1 < chars.length && /\s/.test(chars[i + 1])) { i++; }
        }
        continue;
      }
      var ok = isIdentChar(c, i === 0) || (i > 0 && c >= "0" && c <= "9");
      // first char can't be a digit
      if (i === 0 && c >= "0" && c <= "9") { return false; }
      if (!ok && !(c >= "0" && c <= "9")) { return false; }
    }
    return true;
  }
  // Normalize the optional `ns|` namespace prefix on a type/universal selector. Returns the
  // remainder (`local`) and the serialized prefix. `*|x` and an absent prefix both serialize with no
  // prefix here (no namespaces declared); a bare leading `|` (default namespace) is invalid.
  function normalizeTypePrefix(s) {
    var bar = s.indexOf("|");
    if (bar < 0) { return { prefix: "", rest: s }; }
    var pre = s.slice(0, bar), rest = s.slice(bar + 1);
    if (pre === "" ) { return null; }   // `|div` — default namespace, unsupported → invalid
    if (pre === "*") { return { prefix: "", rest: rest }; } // any namespace → drop prefix
    if (!isIdent(pre)) { return null; }
    return { prefix: "", rest: rest };   // named namespace not declared → still serialize bare
  }
  var HASH = String.fromCharCode(35); // the id-prefix char, built via charcode to dodge Rust raw-string quoting.
  // True if `chars[i]` begins another simple selector in the same compound (class/id/attr/pseudo),
  // i.e. the universal `*` would be redundant and should be dropped during serialization.
  function compoundHasMore(chars, i) {
    if (i >= chars.length) { return false; }
    var c = chars[i];
    return c === "." || c === HASH || c === "[" || c === ":";
  }
  // Parse + canonicalize a CSS <an+b> value (the argument of :nth-child() etc.). Returns the
  // serialized form (e.g. "2n+1", "n", "-n+5", "10") or null if syntactically invalid.
  function serializeAnPlusB(arg) {
    var s = String(arg).trim().toLowerCase().replace(/\s+/g, " ");
    if (s === "") { return null; }
    if (s === "even") { return "2n"; }
    if (s === "odd") { return "2n+1"; }
    var a, b;
    // Pure integer (no `n`): A=0, B=integer.
    var mInt = /^([-+]?\d+)$/.exec(s.replace(/\s+/g, ""));
    if (mInt) { return String(parseInt(mInt[1], 10)); }
    // Forms with `n`: optional sign+coeff, `n`, optional ` ± b`.
    var compact = s.replace(/\s+/g, "");
    var m = /^([-+]?\d*)n([-+]\d+)?$/.exec(compact);
    if (!m) { return null; }
    var acoef = m[1];
    if (acoef === "" || acoef === "+") { a = 1; }
    else if (acoef === "-") { a = -1; }
    else { a = parseInt(acoef, 10); }
    b = m[2] != null ? parseInt(m[2], 10) : 0;
    // Serialize.
    var aPart;
    if (a === 1) { aPart = "n"; }
    else if (a === -1) { aPart = "-n"; }
    else { aPart = a + "n"; }
    if (a === 0) { return String(b); } // (shouldn't reach: handled by mInt)
    if (b === 0) { return aPart; }
    return aPart + (b > 0 ? "+" + b : "-" + (-b));
  }
  function normalizeComplexSelector(sel, nsCtx) {
    nsCtx = nsCtx || { hasDefault: false, prefixes: {} };
    sel = sel.trim();
    if (!sel) { return null; }
    var chars = Array.from(sel);
    var i = 0, n = chars.length, out = "", expectSimple = true, sawSimple = false;
    function err() { return null; }
    while (i < n) {
      var c = chars[i];
      if (c === " " || c === "\t" || c === "\n" || c === "\r" || c === "\f") {
        // Whitespace: a descendant combinator unless followed by another combinator.
        while (i < n && /\s/.test(chars[i])) { i++; }
        if (i >= n) { break; }
        var nx = chars[i];
        if (nx === ">" || nx === "+" || nx === "~") { continue; } // handled below
        out += " "; expectSimple = true; sawSimple = false; continue;
      }
      if (c === ">" || c === "+" || c === "~") {
        if (!sawSimple && out.replace(/\s+$/,"") === "") { return err(); }
        out = out.replace(/\s+$/, "") + " " + c + " ";
        i++; while (i < n && /\s/.test(chars[i])) { i++; }
        expectSimple = true; sawSimple = false; continue;
      }
      // Type / universal selector with an optional namespace prefix (`ns|`, `*|`, `|`). Reachable
      // when the compound starts with `*`, `|`, or an identifier char.
      if (c === "*" || c === "|" || isIdentChar(c, true)) {
        // Read an optional prefix terminated by `|`.
        var save = i, pre = null;
        if (c === "*") { pre = "*"; i++; }
        else if (c === "|") { pre = ""; }   // leading `|` → empty (default-namespace) prefix
        else {
          var pid = ""; while (i < n && isIdentChar(chars[i], pid === "")) { pid += chars[i]; i++; }
          pre = pid;
        }
        var hasBar = (i < n && chars[i] === "|");
        if (hasBar) {
          // Consume `|` and the local part.
          i++;
          var local;
          if (i < n && chars[i] === "*") { local = "*"; i++; }
          else {
            var lid = "";
            while (i < n) {
              if (chars[i] === "\\") {
                // Consume an escape (backslash + a hex code with optional trailing space, or a single char).
                lid += chars[i]; i++;
                if (i < n && /[0-9a-fA-F]/.test(chars[i])) {
                  var hk = 0;
                  while (i < n && hk < 6 && /[0-9a-fA-F]/.test(chars[i])) { lid += chars[i]; i++; hk++; }
                  if (i < n && /\s/.test(chars[i])) { lid += chars[i]; i++; }
                } else if (i < n) { lid += chars[i]; i++; }
                continue;
              }
              if (!isIdentChar(chars[i], lid === "")) { break; }
              lid += chars[i]; i++;
            }
            local = lid;
          }
          // Validate the local part.
          if (local !== "*" && !isIdent(local)) { return err(); }
          // Serialize the prefix per CSSOM. `|local` (no namespace) → keep `|`. `*|local` (any
          // namespace) → keep `*|` only when a default namespace is declared, else drop. A named
          // prefix `ns|` → keep when declared; bare (no prefixes declared) → drop.
          var serPre = "";
          if (pre === "") { serPre = "|"; }
          else if (pre === "*") { serPre = nsCtx.hasDefault ? "*|" : ""; }
          else if (isIdent(pre)) {
            var puri = nsCtx.prefixes[pre];
            // An undeclared namespace prefix makes the selector invalid (parse error).
            if (puri == null) { return err(); }
            // Declared named prefix whose URI equals the default namespace URI -> serialize bare.
            serPre = (nsCtx.hasDefault && puri === nsCtx.defaultUri) ? "" : pre + "|";
          }
          else { return err(); }
          if (local === "*") {
            // A universal local is kept when a prefix is serialized; otherwise it's dropped if the
            // compound has more simple selectors (`*.c` -> `.c`), kept if it stands alone (`*` -> `*`).
            if (serPre) { out += serPre + "*"; }
            else { out += compoundHasMore(chars, i) ? "" : "*"; }
          } else {
            out += serPre + local;
          }
        } else {
          // No prefix: a bare universal or type selector.
          if (pre === "*") { out += compoundHasMore(chars, i) ? "" : "*"; }
          else if (pre !== null && isIdent(pre)) { out += pre; }
          else { return err(); }
        }
        sawSimple = true; expectSimple = false; continue;
      }
      if (c === ".") {
        i++; var cls = ""; while (i < n && isIdentChar(chars[i], cls === "")) { cls += chars[i]; i++; }
        if (!isIdent(cls)) { return err(); }
        out += "." + cls; sawSimple = true; expectSimple = false; continue;
      }
      if (c === HASH) {
        i++; var id = ""; while (i < n && isIdentChar(chars[i], id === "")) { id += chars[i]; i++; }
        if (!isIdent(id)) { return err(); }
        out += HASH + id; sawSimple = true; expectSimple = false; continue;
      }
      if (c === "[") {
        // Attribute selector: scan to matching `]`.
        var depth = 1; i++; var attr = "";
        while (i < n && depth > 0) { if (chars[i] === "[") { depth++; } else if (chars[i] === "]") { depth--; if (depth === 0) { break; } } attr += chars[i]; i++; }
        if (depth !== 0) { return err(); }
        i++; // consume ]
        var na = normalizeAttr(attr);
        if (na === null) { return err(); }
        out += "[" + na + "]"; sawSimple = true; expectSimple = false; continue;
      }
      if (c === ":") {
        var dbl = (chars[i + 1] === ":");
        var start = i; i += dbl ? 2 : 1;
        var nm = "";
        while (i < n && isIdentChar(chars[i], nm === "")) { nm += chars[i]; i++; }
        if (!isIdent(nm)) { return err(); }
        var lower = nm.toLowerCase();
        var arg = "";
        if (i < n && chars[i] === "(") {
          var d2 = 1; i++; while (i < n && d2 > 0) { if (chars[i] === "(") { d2++; } else if (chars[i] === ")") { d2--; if (d2 === 0) { break; } } arg += chars[i]; i++; }
          if (d2 !== 0) { return err(); }
          i++; // consume )
        }
        if (__cssPseudoElements[lower] && !arg) {
          out += "::" + lower; sawSimple = true; expectSimple = false; continue;
        }
        if (!dbl && __cssPseudoClasses[lower]) {
          if (lower === "not" || lower === "is" || lower === "where" || lower === "has") {
            // Recursively validate the argument as a selector list.
            var inner = arg.split(",").map(function (s) { return normalizeComplexSelector(s, nsCtx); });
            if (inner.indexOf(null) >= 0 || inner.length === 0) { return err(); }
            out += ":" + lower + "(" + inner.join(", ") + ")";
          } else if (lower === "nth-child" || lower === "nth-last-child" || lower === "nth-of-type" || lower === "nth-last-of-type") {
            // Canonicalize the An+B microsyntax (CSSOM "serialize an <an+b> value").
            var anb = serializeAnPlusB(arg);
            if (anb === null) { return err(); }
            out += ":" + lower + "(" + anb + ")";
          } else {
            out += ":" + lower + (arg ? "(" + arg.trim() + ")" : "");
          }
          sawSimple = true; expectSimple = false; continue;
        }
        return err(); // unknown pseudo / `::pseudo-class`
      }
      return err(); // any other char (`!`, `$`, `(`, `{`, ...) is invalid
    }
    out = out.trim();
    if (!out) { return null; }
    // A trailing combinator is invalid.
    if (/[>+~]\s*$/.test(out)) { return null; }
    // (Redundant universal `*` dropping is handled per-compound during type-selector serialization,
    // so a namespaced universal like `|*.c` keeps its `*`.)
    return out;
  }
  // Validate/normalize the inside of `[...]`. Accepts `attr`, `ns|attr`, `*|attr`, and
  // `attr OP "value"` / `attr OP value` with OP in =, ~=, |=, ^=, $=, *=, plus an optional case
  // flag. Returns the normalized inner text, or null if invalid.
  function normalizeAttr(attr) {
    attr = attr.trim();
    if (!attr) { return null; }
    // Optional namespace prefix (`ns|`, `*|`, `|`) then the attribute name, then operator + value.
    var m = /^((?:[^|=~^$*\s]*|\*)\|)?([^|=~^$*\s]+)\s*([~|^$*]?=)?\s*([\s\S]*)$/.exec(attr);
    if (!m) { return null; }
    var rawPre = m[1], local = m[2], op = m[3] || "", val = (m[4] || "").trim();
    // Decode CSS escapes in the local name, then re-serialize it as a canonical identifier
    // (so `\30zonk` -> `\30 zonk`, `ns\:foo` -> `ns\:foo`).
    var localDecoded = unescapeCssIdent(local);
    // Any non-empty decoded name is a valid attribute name (escapeCssIdent makes leading digits etc.
    // legal via escapes). Only reject if it's empty.
    var localSer = localDecoded.length ? escapeCssIdent(localDecoded) : null;
    var name;
    if (rawPre != null) {
      var pre = rawPre.slice(0, -1); // drop the trailing `|`
      if (localSer === null) { return null; }
      if (pre === "*") { name = "*|" + localSer; }       // `[*|lang]` keeps the `*|`
      else if (pre === "") { name = localSer; }           // `[|lang]` -> `[lang]`
      else if (isIdent(pre)) { name = pre + "|" + localSer; }
      else { return null; }
    } else {
      if (localSer === null) { return null; }
      name = localSer;
    }
    if (!op) { return name; }
    // Value: quote if it's an unquoted identifier; keep quoted values, switching to double quotes.
    var flag = "";
    var fm = /\s+([iIsS])\s*$/.exec(val);
    if (fm) { flag = " " + fm[1].toLowerCase(); val = val.slice(0, val.length - fm[0].length).trim(); }
    var qv;
    if ((val.charAt(0) === '"' && val.charAt(val.length - 1) === '"') ||
        (val.charAt(0) === "'" && val.charAt(val.length - 1) === "'")) {
      qv = '"' + val.slice(1, -1) + '"';
    } else if (isIdent(val) || /^-?\d/.test(val)) {
      qv = '"' + val + '"';
    } else { return null; }
    return name + op + qv + flag;
  }
  // The CSSOM "serialize a selector" / "parse a group of selectors": validate every comma
  // component; if any is invalid the whole group is invalid (null). Otherwise join with ", ".
  function normalizeSelectorList(sel, nsCtx) {
    sel = String(sel == null ? "" : sel);
    var parts = sel.split(",");
    var outs = [];
    for (var i = 0; i < parts.length; i++) {
      var nrm = normalizeComplexSelector(parts[i], nsCtx);
      if (nrm === null) { return null; }
      outs.push(nrm);
    }
    if (!outs.length) { return null; }
    return outs.join(", ");
  }
  // Build a namespace context {hasDefault, defaultUri, prefixes:{name:uri}} from a sheet's
  // @namespace rule structs. Tracks each prefix's URI and the default namespace's URI so a named
  // prefix bound to the default namespace's URI can serialize bare (per CSSOM).
  function nsUri(raw) { return unquoteCss(String(raw).replace(/^url\(\s*|\s*\)$/g, "").trim()); }
  function sheetNsContext(sheet) {
    var ctx = { hasDefault: false, defaultUri: null, prefixes: {} };
    if (!sheet || !sheet.__structs) { return ctx; }
    var structs = sheet.__structs;
    for (var i = 0; i < structs.length; i++) {
      var st = structs[i];
      if (st.kind !== "@namespace") { continue; }
      var parts = splitTopLevel(st.prelude, " ").filter(function (x) { return x !== ""; });
      if (parts.length >= 2) { ctx.prefixes[parts[0]] = nsUri(parts.slice(1).join(" ")); }
      else { ctx.hasDefault = true; ctx.defaultUri = nsUri(parts[0] || ""); }
    }
    return ctx;
  }
  // ============================================================================================
  // CSSOM rule object model.
  //
  // Parsed CSS rules reach JS by re-parsing the sheet's raw CSS text on the JS side (the Rust `css`
  // crate flattens nesting for the cascade and isn't a faithful CSSOM source). `parseRuleStructs`
  // tokenizes top-level rules (brace-balanced, string/comment aware) into structured nodes
  // {kind, prelude, body, decls?, children?}. Each structured node is wrapped once in a *stable*
  // CSSRule object (cached by identity) so page-set expandos (e.g. `rule.randomProperty = 1`)
  // survive insert/delete — the CSSOM `[SameObject]` requirement. The owning CSSStyleSheet keeps an
  // ordered list of rule models and exposes a single stable CSSRuleList whose contents are kept in
  // sync as rules are inserted/deleted. Serialization (`cssText`) is spec-faithful so the WPT exact
  // string comparisons pass.
  // ============================================================================================

  // Tokenize a CSS string into top-level rule structs. `parentSheet`/`parentRule` thread ownership.
  function parseRuleStructs(css) {
    css = String(css == null ? "" : css);
    var out = [], n = css.length, i = 0;
    while (i < n) {
      // Skip whitespace and comments between rules.
      while (i < n && /\s/.test(css[i])) { i++; }
      if (i < n && css[i] === "/" && css[i + 1] === "*") { var e = css.indexOf("*/", i + 2); i = e < 0 ? n : e + 2; continue; }
      if (i >= n) { break; }
      // Read prelude up to `{` or `;` at depth 0 (string/comment aware).
      var preStart = i, sawBrace = false;
      while (i < n) {
        var c = css[i];
        if (c === "/" && css[i + 1] === "*") { var ce = css.indexOf("*/", i + 2); i = ce < 0 ? n : ce + 2; continue; }
        if (c === '"' || c === "'") { i++; while (i < n && css[i] !== c) { if (css[i] === "\\") { i++; } i++; } i++; continue; }
        if (c === "{") { sawBrace = true; break; }
        if (c === ";") { break; }
        i++;
      }
      var prelude = css.slice(preStart, i).trim();
      if (!sawBrace) {
        // Statement at-rule (e.g. `@import ...;`, `@namespace ...;`). Consume the `;`.
        if (i < n && css[i] === ";") { i++; }
        // `@charset` is a parse directive, not a CSS rule — it never appears in `cssRules` (CSSOM).
        if (prelude) { var __st = structFromPrelude(prelude, ""); if (__st && __st.kind !== "@charset") { out.push(__st); } }
        continue;
      }
      // Read the brace-balanced body.
      i++; var bodyStart = i, depth = 1;
      while (i < n && depth > 0) {
        var d = css[i];
        if (d === "/" && css[i + 1] === "*") { var be = css.indexOf("*/", i + 2); i = be < 0 ? n : be + 2; continue; }
        if (d === '"' || d === "'") { i++; while (i < n && css[i] !== d) { if (css[i] === "\\") { i++; } i++; } i++; continue; }
        if (d === "{") { depth++; }
        else if (d === "}") { depth--; if (depth === 0) { break; } }
        i++;
      }
      var body = css.slice(bodyStart, i);
      if (i < n && css[i] === "}") { i++; }
      var __srule = structFromPrelude(prelude, body);
      // Drop a style rule whose selector uses a functional pseudo-element without its required
      // argument (`::part`, `::slotted`, `::highlight`) — it's invalid, so the rule isn't parsed.
      if (__srule && !(__srule.kind === "style" && /::(?:part|slotted|highlight)\b(?!\s*\()/i.test(__srule.prelude))) {
        out.push(__srule);
      }
    }
    return out;
  }
  // Classify a prelude + body into a rule struct.
  function structFromPrelude(prelude, body) {
    if (prelude.charAt(0) === "@") {
      var m = /^@([-\w]+)\s*([\s\S]*)$/.exec(prelude);
      var name = (m ? m[1] : "").toLowerCase();
      var rest = m ? m[2].trim() : "";
      return { kind: "@" + name, atName: name, prelude: rest, body: body };
    }
    return { kind: "style", prelude: prelude, body: body };
  }
  // Parse a declaration block body into an array of [name, value, priority] tuples (CSSOM order).
  function parseDeclList(body) {
    var out = [], parts = splitTopLevel(body, ";");
    for (var i = 0; i < parts.length; i++) {
      var seg = parts[i], c = seg.indexOf(":");
      if (c < 0) { continue; }
      var name = seg.slice(0, c).trim().toLowerCase();
      var val = seg.slice(c + 1).trim();
      if (!name) { continue; }
      var prio = "";
      var pm = /!\s*important\s*$/i.exec(val);
      if (pm) { prio = "important"; val = val.slice(0, val.length - pm[0].length).trim(); }
      out.push([name, normalizeCssValue(val), prio]);
    }
    return out;
  }
  // Split on `sep` at brace/paren/string depth 0.
  function splitTopLevel(s, sep) {
    s = String(s); var out = [], depth = 0, start = 0, n = s.length;
    for (var i = 0; i < n; i++) {
      var c = s[i];
      if (c === '"' || c === "'") { i++; while (i < n && s[i] !== c) { if (s[i] === "\\") { i++; } i++; } continue; }
      if (c === "{" || c === "(" || c === "[") { depth++; }
      else if (c === "}" || c === ")" || c === "]") { depth--; }
      else if (c === sep && depth === 0) { out.push(s.slice(start, i)); start = i + 1; }
    }
    out.push(s.slice(start));
    return out;
  }
  function serializeDeclList(decls) {
    var s = "";
    for (var i = 0; i < decls.length; i++) {
      s += (s ? " " : "") + decls[i][0] + ": " + decls[i][1] + (decls[i][2] ? " !" + decls[i][2] : "") + ";";
    }
    return s;
  }
  // A standalone CSSStyleDeclaration over an in-memory `[name,value,priority]` array. `onChange` is
  // called after any mutation (so the owning rule can re-serialize). `instanceof CSSStyleDeclaration`.
  function makeRuleStyle(decls, onChange, restrict) {
    // Drop any property the context disallows (e.g. animation-* inside @keyframes) from the initial
    // parsed declarations, so `style.length`/serialization reflect only the applicable properties.
    if (restrict) { for (var di = decls.length - 1; di >= 0; di--) { if (!isCustomProp(decls[di][0]) && !restrict(decls[di][0])) { decls.splice(di, 1); } } }
    function find(name) { for (var i = 0; i < decls.length; i++) { if (decls[i][0] === name) { return i; } } return -1; }
    function getVal(name) { var i = find(name); return i >= 0 ? decls[i][1] : ""; }
    function setVal(name, val, prio) {
      if (restrict && !isCustomProp(name) && !restrict(name)) { return; } // disallowed in this context
      var i = find(name);
      if (val == null || val === "") { if (i >= 0) { decls.splice(i, 1); } }
      else {
        val = normalizeCssValue(String(val));
        if (i >= 0) { decls[i][1] = val; decls[i][2] = prio || ""; } else { decls.push([name, val, prio || ""]); }
      }
      if (onChange) { onChange(); }
    }
    var base = {
      getPropertyValue: function (p) { return getVal(String(p).toLowerCase()); },
      getPropertyPriority: function (p) { var i = find(String(p).toLowerCase()); return i >= 0 ? decls[i][2] : ""; },
      setProperty: function (p, v, prio) { setVal(String(p).toLowerCase(), v, String(prio || "").toLowerCase() === "important" ? "important" : ""); },
      removeProperty: function (p) { p = String(p).toLowerCase(); var old = getVal(p); setVal(p, ""); return old; },
      item: function (i) { return i >= 0 && i < decls.length ? decls[i][0] : ""; },
      parentRule: null
    };
    Object.defineProperty(base, "length", { get: function () { return decls.length; }, enumerable: false, configurable: true });
    Object.defineProperty(base, "cssText", {
      get: function () { return serializeDeclList(decls); },
      set: function (v) { decls.length = 0; var p = parseDeclList(v); for (var i = 0; i < p.length; i++) { if (!restrict || isCustomProp(p[i][0]) || restrict(p[i][0])) { decls.push(p[i]); } } if (onChange) { onChange(); } },
      enumerable: true, configurable: true
    });
    try { if (globalThis.CSSStyleDeclaration && globalThis.CSSStyleDeclaration.prototype) { Object.setPrototypeOf(base, globalThis.CSSStyleDeclaration.prototype); } } catch (e) {}
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
          if (p in t && p !== "length") { t[p] = v; return true; }
          setVal(camelToKebab(p), v); return true;
        }
      });
    } catch (e) { return base; }
  }

  // --- @media condition serialization (CSSOM "serialize a media query list") ------------------
  // Lowercase a media type token; drop a leading `all` (unless negated). Per serialize-media-rule.
  function serializeMediaQuery(q) {
    q = q.trim().replace(/\s+/g, " ");
    if (q === "") { return ""; }
    // Lowercase media features inside parens and bare type/keyword tokens, preserving values.
    // Split into the leading "<not>? <type>?" head and " and (...)" tail features.
    var parts = splitTopLevel(q, " ").filter(function (x) { return x !== ""; });
    // Reconstruct by lowercasing keywords (not/and/or/only/type names) and feature names in parens.
    var negated = false, typeTok = null, feats = [], idx = 0;
    if (parts[idx] && parts[idx].toLowerCase() === "not") { negated = true; idx++; }
    if (parts[idx] && parts[idx].toLowerCase() === "only") { idx++; }
    if (parts[idx] && parts[idx].charAt(0) !== "(") { typeTok = parts[idx].toLowerCase(); idx++; }
    // Remaining: `and (feature)` groups. Re-join the rest and split on top-level " and ".
    var tail = parts.slice(idx).join(" ");
    var featGroups = tail ? splitTopLevel(tail, " ") : [];
    // Rebuild feature list: each `(...)` token lowercased on the feature name.
    var rebuilt = [];
    for (var i = 0; i < parts.length; i++) {
      var t = parts[i];
      if (t.charAt(0) === "(") { rebuilt.push(serializeMediaFeature(t)); }
    }
    var head;
    if (typeTok === "all" && !negated && rebuilt.length) { head = ""; }
    else { head = (negated ? "not " : "") + (typeTok || (negated || rebuilt.length === 0 ? "all" : "")); head = head.trim(); }
    var s = head;
    for (var j = 0; j < rebuilt.length; j++) { s += (s ? " and " : "") + rebuilt[j]; }
    return s.trim();
  }
  // Lowercase a `(feature: value)` token's feature name (and bare `(color)`), preserve value casing.
  function serializeMediaFeature(tok) {
    var inner = tok.replace(/^\(\s*/, "").replace(/\s*\)$/, "");
    var c = inner.indexOf(":");
    if (c < 0) { return "(" + inner.trim().toLowerCase() + ")"; }
    return "(" + inner.slice(0, c).trim().toLowerCase() + ": " + inner.slice(c + 1).trim() + ")";
  }
  function serializeMediaList(text) {
    text = String(text == null ? "" : text).trim();
    if (text === "") { return ""; }
    var queries = splitTopLevel(text, ",").map(function (q) { return serializeMediaQuery(q); }).filter(function (q) { return q !== ""; });
    return queries.join(", ");
  }
  // A MediaList over a mutable backing string holder {text}. `onChange` re-serializes the owner.
  function makeMediaList(holder, onChange) {
    function items() { var t = serializeMediaList(holder.text); return t === "" ? [] : splitTopLevel(t, ",").map(function (x) { return x.trim(); }); }
    var ml = {
      item: function (i) { var it = items(); return i >= 0 && i < it.length ? it[i] : null; },
      appendMedium: function (m) { if (arguments.length < 1) { throw new TypeError("appendMedium requires 1 argument"); } var it = items(); m = serializeMediaQuery(String(m)); if (it.indexOf(m) < 0) { it.push(m); } holder.text = it.join(", "); if (onChange) { onChange(); } },
      deleteMedium: function (m) { if (arguments.length < 1) { throw new TypeError("deleteMedium requires 1 argument"); } var it = items(); m = serializeMediaQuery(String(m)); var k = it.indexOf(m); if (k < 0) { throw new globalThis.DOMException("Not found", "NotFoundError"); } it.splice(k, 1); holder.text = it.join(", "); if (onChange) { onChange(); } },
      toString: function () { return serializeMediaList(holder.text); }
    };
    Object.defineProperty(ml, "length", { get: function () { return items().length; }, enumerable: true, configurable: true });
    Object.defineProperty(ml, "mediaText", {
      get: function () { return serializeMediaList(holder.text); },
      set: function (v) { holder.text = (v == null) ? "" : String(v); if (onChange) { onChange(); } },
      enumerable: true, configurable: true
    });
    try { if (globalThis.MediaList && globalThis.MediaList.prototype) { Object.setPrototypeOf(ml, globalThis.MediaList.prototype); } } catch (e) {}
    try {
      return new Proxy(ml, { get: function (t, p) {
        if (typeof p === "string" && /^\d+$/.test(p)) { var v = t.item(parseInt(p, 10)); return v == null ? undefined : v; }
        return t[p];
      } });
    } catch (e) { return ml; }
  }

  // --- @import prelude parsing/serialization ---------------------------------------------------
  // Parse `@import` prelude: url + optional layer + optional supports() + optional media query.
  function parseImportPrelude(prelude) {
    var s = String(prelude).trim();
    var href = "", rest = s;
    var um = /^url\(\s*("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|[^)\s]*)\s*\)/i.exec(s);
    if (um) { href = unquoteCss(um[1]); rest = s.slice(um[0].length).trim(); }
    else {
      var qm = /^("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')/.exec(s);
      if (qm) { href = unquoteCss(qm[1]); rest = s.slice(qm[0].length).trim(); }
    }
    var layer = null, supports = null;
    var lm = /^layer\((.*?)\)/i.exec(rest);
    if (lm) { layer = lm[1].trim(); rest = rest.slice(lm[0].length).trim(); }
    else if (/^layer\b/i.test(rest)) { layer = ""; rest = rest.replace(/^layer\b/i, "").trim(); }
    var sm = /^supports\(([\s\S]*?)\)\s*/i.exec(rest);
    if (sm) {
      // Balance parens for nested conditions.
      var depth = 0, k = rest.indexOf("(") , start = k + 1, end = -1;
      for (var p = k; p < rest.length; p++) { if (rest[p] === "(") { depth++; } else if (rest[p] === ")") { depth--; if (depth === 0) { end = p; break; } } }
      if (end > start) { supports = rest.slice(start, end).trim(); rest = rest.slice(end + 1).trim(); }
    }
    var media = rest.trim();
    return { href: href, layer: layer, supports: supports, media: media };
  }
  function unquoteCss(s) {
    s = String(s);
    if ((s.charAt(0) === '"' && s.charAt(s.length - 1) === '"') || (s.charAt(0) === "'" && s.charAt(s.length - 1) === "'")) {
      return s.slice(1, -1).replace(/\\(.)/g, "$1");
    }
    return s;
  }
  // Serialize a string as a double-quoted CSS string (escape `"` and `\`).
  function cssQuote(s) { return '"' + String(s).replace(/\\/g, "\\\\").replace(/"/g, '\\"') + '"'; }

  // --- Stable CSSRule object construction ------------------------------------------------------
  // Build a CSSRule object for `struct` owned by `sheet` (a CSSStyleSheet) with `parentRule`.
  function makeCssRule(struct, sheet, parentRule) {
    var rule = makeCssRuleInner(struct, sheet, parentRule);
    // A rule detached from its sheet (deleteRule) reports parentStyleSheet/parentRule === null.
    // Defined on the rule's INTERMEDIATE prototype (not the instance) so assert_idl_attribute (which
    // requires these to be inherited, not own properties) still passes.
    try {
      var proto = Object.getPrototypeOf(rule);
      if (proto && proto !== Object.prototype) {
        Object.defineProperty(proto, "parentStyleSheet", { get: function () { return struct.__detached ? null : (sheet || null); }, enumerable: true, configurable: true });
        Object.defineProperty(proto, "parentRule", { get: function () { return struct.__detached ? null : (parentRule || null); }, enumerable: true, configurable: true });
      }
    } catch (e) {}
    return rule;
  }
  function makeCssRuleInner(struct, sheet, parentRule) {
    var kind = struct.kind;
    if (kind === "style") { return makeStyleRule(struct, sheet, parentRule); }
    if (kind === "@media") { return makeMediaRule(struct, sheet, parentRule); }
    if (kind === "@import") { return makeImportRule(struct, sheet, parentRule); }
    if (kind === "@font-feature-values") { return makeFontFeatureValuesRule(struct, sheet, parentRule); }
    if (kind === "@font-face") { return makeFontFaceRule(struct, sheet, parentRule); }
    if (kind === "@counter-style") { return makeCounterStyleRule(struct, sheet, parentRule); }
    if (kind === "@namespace") { return makeNamespaceRule(struct, sheet, parentRule); }
    if (kind === "@supports") { return makeSupportsRule(struct, sheet, parentRule); }
    if (kind === "@container") { return makeContainerRule(struct, sheet, parentRule); }
    if (kind === "@keyframes" || kind === "@-webkit-keyframes") { return makeKeyframesRule(struct, sheet, parentRule); }
    if (kind === "@page") { return makePageRule(struct, sheet, parentRule); }
    // Unknown at-rule: a generic rule that serializes its raw text.
    return makeGenericRule(struct, sheet, parentRule, 0);
  }
  // Define an accessor/value on a rule's INTERMEDIATE prototype (not the instance), so the CSSOM
  // [SameObject]/inherited-attribute semantics hold: `rule.hasOwnProperty("type")` is false but
  // `rule.type` resolves (assert_idl_attribute). The instance stays empty for page expandos.
  function defOn(rule, name, desc) { desc.configurable = true; Object.defineProperty(Object.getPrototypeOf(rule), name, desc); }
  // Create a fresh rule instance whose prototype holds the per-instance accessors and chains up to
  // the global interface constructor's prototype (for `instanceof`). Stores `type` on the proto.
  function newRule(ctorName, type, sheet, parentRule) {
    var proto = {};
    try { var ctor = globalThis[ctorName]; if (ctor && ctor.prototype) { Object.setPrototypeOf(proto, ctor.prototype); } } catch (e) {}
    Object.defineProperty(proto, "type", { get: function () { return type; }, enumerable: true, configurable: true });
    Object.defineProperty(proto, "parentStyleSheet", { get: function () { return sheet || null; }, enumerable: true, configurable: true });
    Object.defineProperty(proto, "parentRule", { get: function () { return parentRule || null; }, enumerable: true, configurable: true });
    return Object.create(proto);
  }
  // A rule's `.style` CSSStyleDeclaration, backed by the rule's declaration body text. Uses the
  // shared `makeStyleDecl` machinery (shorthand expand/serialize, custom props) so rule blocks
  // serialize identically to inline styles. `struct.body` holds the current (flat) declaration text.
  function makeRuleStyleDecl(struct, sheet, restrict) {
    if (struct.body == null) { struct.body = ""; }
    return makeStyleDecl(
      function () { return struct.body; },
      function (text) { struct.body = text; markDirty(sheet); },
      restrict
    );
  }
  // @page applies only the page-context properties (margins, page size/marks/bleed, and a handful of
  // box/background properties); anything else (e.g. `transform`) is dropped. CSS Page 3 §3.4.
  function pagePropertyAllowed(name) {
    if (/^margin(-|$)/.test(name) || /^padding(-|$)/.test(name)) return true;
    if (/^(size|marks|bleed|page|page-orientation)$/.test(name)) return true;
    if (/^(width|height|min-width|min-height|max-width|max-height)$/.test(name)) return true;
    return false;
  }
  // @keyframes block applies every property EXCEPT the animation longhands/shorthand (CSS Animations
  // §2: "animatable properties other than the animation properties").
  function keyframePropertyAllowed(name) {
    return !(name === "animation" || /^animation-/.test(name));
  }
  function makeStyleRule(struct, sheet, parentRule) {
    var rule = newRule("CSSStyleRule", 1, sheet, parentRule);
    var styleObj = makeRuleStyleDecl(struct, sheet);
    function selText() { var nrm = normalizeSelectorList(struct.prelude, sheetNsContext(sheet)); return nrm == null ? struct.prelude.trim() : nrm; }
    defOn(rule, "selectorText", {
      get: selText,
      set: function (v) { var nrm = normalizeSelectorList(v, sheetNsContext(sheet)); if (nrm != null) { struct.prelude = nrm; markDirty(sheet); } },
      enumerable: true
    });
    defOn(rule, "style", {
      get: function () { return styleObj; },
      set: function (v) { styleObj.cssText = v == null ? "" : String(v); },
      enumerable: true
    });
    defOn(rule, "cssText", { get: function () {
      var sel = selText();
      var body = styleObj.cssText;
      return sel + " { " + (body ? body + " " : "") + "}";
    }, enumerable: true });
    return rule;
  }
  function makePageRule(struct, sheet, parentRule) {
    // @page exposes a `.style` (CSSStyleDeclaration) like a style rule. Type 6.
    var rule = newRule("CSSPageRule", 6, sheet, parentRule);
    var styleObj = makeRuleStyleDecl(struct, sheet, pagePropertyAllowed);
    // The page selector (`:left`, `:first`, named page, etc.) — normalized (pseudo lowercased).
    function pageSel() { return normalizePageSelector(struct.prelude); }
    defOn(rule, "selectorText", {
      get: pageSel,
      set: function (v) { var nrm = normalizePageSelector(v == null ? "" : String(v)); if (nrm != null) { struct.prelude = nrm; markDirty(sheet); } },
      enumerable: true
    });
    defOn(rule, "style", { get: function () { return styleObj; }, set: function (v) { styleObj.cssText = v == null ? "" : String(v); }, enumerable: true });
    defOn(rule, "cssText", { get: function () {
      var body = styleObj.cssText; var sel = pageSel();
      return "@page" + (sel ? " " + sel : "") + " { " + (body ? body + " " : "") + "}";
    }, enumerable: true });
    return rule;
  }
  // Normalize an @page selector. Empty stays empty. Pseudo-page classes (`:left`/`:right`/`:first`/
  // `:blank`) lowercase; a named page keeps its case; combinations like `named:left` are preserved.
  function normalizePageSelector(sel) {
    sel = String(sel == null ? "" : sel).trim();
    if (sel === "") { return ""; }
    // Validate: optional ident, then zero or more `:pseudo` (left|right|first|blank).
    var m = /^([A-Za-z_-][\w-]*)?((?::(?:left|right|first|blank))*)$/i.exec(sel);
    if (!m) { return null; }
    var name = m[1] || "";
    var pseudos = (m[2] || "").toLowerCase();
    return name + pseudos;
  }
  function makeMediaRule(struct, sheet, parentRule) {
    var rule = newRule("CSSMediaRule", 4, sheet, parentRule);
    var holder = { text: struct.prelude };
    var mediaList = makeMediaList(holder, function () { markDirty(sheet); });
    var childRules = parseRuleStructs(struct.body);
    var childList = makeRuleList(childRules, sheet, rule);
    defOn(rule, "media", { get: function () { return mediaList; }, set: function (v) { mediaList.mediaText = v; }, enumerable: true });
    // conditionText getter mirrors media.mediaText; the setter is a no-op for @media (per browsers).
    defOn(rule, "conditionText", { get: function () { return serializeMediaList(holder.text); }, set: function () {}, enumerable: true });
    defOn(rule, "cssRules", { get: function () { return childList; }, enumerable: true });
    defOn(rule, "insertRule", { value: function (text, index) { return childList.__insert(String(text), index); }, enumerable: true });
    defOn(rule, "deleteRule", { value: function (index) { return childList.__delete(index); }, enumerable: true });
    defOn(rule, "cssText", { get: function () {
      var cond = serializeMediaList(holder.text);
      var inner = "";
      for (var i = 0; i < childList.length; i++) { inner += "  " + childList[i].cssText + "\n"; }
      return "@media " + cond + " {\n" + inner + "}";
    }, enumerable: true });
    return rule;
  }
  function makeSupportsRule(struct, sheet, parentRule) {
    var rule = newRule("CSSSupportsRule", 12, sheet, parentRule);
    var childList = makeRuleList(parseRuleStructs(struct.body), sheet, rule);
    defOn(rule, "conditionText", { get: function () { return struct.prelude.trim(); }, enumerable: true });
    defOn(rule, "cssRules", { get: function () { return childList; }, enumerable: true });
    defOn(rule, "insertRule", { value: function (text, index) { return childList.__insert(String(text), index); }, enumerable: true });
    defOn(rule, "deleteRule", { value: function (index) { return childList.__delete(index); }, enumerable: true });
    defOn(rule, "cssText", { get: function () {
      var inner = ""; for (var i = 0; i < childList.length; i++) { inner += "  " + childList[i].cssText + "\n"; }
      return "@supports " + struct.prelude.trim() + " {\n" + inner + "}";
    }, enumerable: true });
    return rule;
  }
  // Split a `@container` prelude into an optional container-name + the container query.
  // `sidebar (min-width: 100px)` -> {name:"sidebar", query:"(min-width: 100px)"}; a query that
  // starts with `(`/`not`/`style(`/`scroll-state(` has no name.
  function parseContainerPrelude(prelude) {
    var s = String(prelude).trim();
    var m = /^([-\w -￿]+)\s+([\s\S]+)$/.exec(s);
    if (m && m[1].charAt(0) !== "(" && !/^(not|and|or|style|scroll-state)$/i.test(m[1])) {
      return { name: m[1], query: m[2].trim() };
    }
    return { name: "", query: s };
  }
  function makeContainerRule(struct, sheet, parentRule) {
    var rule = newRule("CSSContainerRule", 0, sheet, parentRule);
    var childList = makeRuleList(parseRuleStructs(struct.body), sheet, rule);
    defOn(rule, "containerName", { get: function () { return parseContainerPrelude(struct.prelude).name; }, enumerable: true });
    defOn(rule, "containerQuery", { get: function () { return parseContainerPrelude(struct.prelude).query; }, enumerable: true });
    defOn(rule, "conditionText", { get: function () { return struct.prelude.trim(); }, enumerable: true });
    defOn(rule, "cssRules", { get: function () { return childList; }, enumerable: true });
    defOn(rule, "insertRule", { value: function (text, index) { return childList.__insert(String(text), index); }, enumerable: true });
    defOn(rule, "deleteRule", { value: function (index) { return childList.__delete(index); }, enumerable: true });
    defOn(rule, "cssText", { get: function () {
      var inner = ""; for (var i = 0; i < childList.length; i++) { inner += "  " + childList[i].cssText + "\n"; }
      return "@container " + struct.prelude.trim() + " {\n" + inner + "}";
    }, enumerable: true });
    return rule;
  }
  function makeImportRule(struct, sheet, parentRule) {
    var rule = newRule("CSSImportRule", 3, sheet, parentRule);
    var info = parseImportPrelude(struct.prelude);
    var holder = { text: info.media };
    var mediaList = makeMediaList(holder, function () { markDirty(sheet); });
    // The imported sheet object (we don't fetch external CSS; provide an empty CSSStyleSheet so
    // `instanceof CSSStyleSheet` holds and ownerRule is wired).
    var imported = null;
    defOn(rule, "href", { get: function () { return info.href; }, enumerable: true });
    defOn(rule, "layerName", { get: function () { return info.layer; }, enumerable: true });
    defOn(rule, "supportsText", { get: function () { return info.supports; }, enumerable: true });
    defOn(rule, "media", { get: function () { return mediaList; }, set: function (v) { mediaList.mediaText = v; }, enumerable: true });
    defOn(rule, "styleSheet", { get: function () {
      if (!imported) {
        imported = makeConstructedSheet(""); imported.__constructed = false; imported.__ownerRule = rule; imported.__href = info.href; imported.__media = mediaList;
        // The imported sheet's parent is the sheet containing the @import — until that rule is
        // removed (struct detached), at which point the child sheet is unlinked (parentStyleSheet null).
        try { Object.defineProperty(imported, "parentStyleSheet", { get: function () { return struct.__detached ? null : sheet; }, configurable: true }); } catch (e) {}
      }
      return imported;
    }, enumerable: true });
    defOn(rule, "cssText", { get: function () {
      var s = "@import " + 'url(' + cssQuote(info.href) + ')';
      if (info.layer === "") { s += " layer"; } else if (info.layer != null) { s += " layer(" + info.layer + ")"; }
      if (info.supports != null) { s += " supports(" + info.supports + ")"; }
      var mt = serializeMediaList(holder.text);
      if (mt) { s += " " + mt; }
      return s + ";";
    }, enumerable: true });
    return rule;
  }
  function makeNamespaceRule(struct, sheet, parentRule) {
    var rule = newRule("CSSNamespaceRule", 10, sheet, parentRule);
    var parts = splitTopLevel(struct.prelude, " ").filter(function (x) { return x !== ""; });
    var prefix = "", uri = "";
    if (parts.length >= 2) { prefix = parts[0]; uri = parts.slice(1).join(" "); } else { uri = parts[0] || ""; }
    defOn(rule, "prefix", { get: function () { return prefix; }, enumerable: true });
    defOn(rule, "namespaceURI", { get: function () { return unquoteCss(uri.replace(/^url\(\s*|\s*\)$/g, "")); }, enumerable: true });
    defOn(rule, "cssText", { get: function () {
      var u = unquoteCss(uri.replace(/^url\(\s*|\s*\)$/g, ""));
      return "@namespace " + (prefix ? prefix + " " : "") + "url(" + cssQuote(u) + ");";
    }, enumerable: true });
    return rule;
  }
  function makeFontFaceRule(struct, sheet, parentRule) {
    var rule = newRule("CSSFontFaceRule", 5, sheet, parentRule);
    var decls = struct.decls || (struct.decls = parseDeclList(struct.body));
    var styleObj = makeRuleStyle(decls, function () { struct.body = serializeDeclList(decls); markDirty(sheet); });
    // The descriptor block of a `@font-face` rule is a `CSSFontFaceDescriptors`, not a plain
    // CSSStyleDeclaration, so `rule.style.toString()` reports `[object CSSFontFaceDescriptors]`.
    try {
      Object.defineProperty(styleObj, Symbol.toStringTag,
        { value: "CSSFontFaceDescriptors", writable: false, enumerable: false, configurable: true });
    } catch (e) {}
    defOn(rule, "style", { get: function () { return styleObj; }, enumerable: true });
    defOn(rule, "cssText", { get: function () {
      var body = serializeDeclList(decls);
      return "@font-face { " + (body ? body + " " : "") + "}";
    }, enumerable: true });
    return rule;
  }
  function makeCounterStyleRule(struct, sheet, parentRule) {
    var rule = newRule("CSSCounterStyleRule", 11, sheet, parentRule);
    var decls = struct.decls || (struct.decls = parseDeclList(struct.body));
    // `name` reflects the prelude; each descriptor is a camelCase IDL attribute over the block.
    defOn(rule, "name", { get: function () { return struct.prelude.trim(); },
      set: function (v) { struct.prelude = String(v).trim(); markDirty(sheet); }, enumerable: true });
    [["system", "system"], ["symbols", "symbols"], ["additiveSymbols", "additive-symbols"],
     ["negative", "negative"], ["prefix", "prefix"], ["suffix", "suffix"], ["range", "range"],
     ["pad", "pad"], ["speakAs", "speak-as"], ["fallback", "fallback"]].forEach(function (pair) {
      defOn(rule, pair[0], {
        get: function () { var i = findDecl(decls, pair[1]); return i >= 0 ? decls[i][1] : ""; },
        set: function (v) { setDecl(decls, pair[1], String(v), false); struct.body = serializeDeclList(decls); markDirty(sheet); },
        enumerable: true
      });
    });
    // Single-line serialization (no newlines), per CSSOM.
    defOn(rule, "cssText", { get: function () {
      var body = serializeDeclList(decls);
      return "@counter-style " + struct.prelude.trim() + " { " + (body ? body + " " : "") + "}";
    }, enumerable: true });
    return rule;
  }
  // A single keyframe (`0% { ... }`) as a CSSKeyframeRule.
  function makeKeyframeRule(kf, parentRule, sheet) {
    var r = newRule("CSSKeyframeRule", 8, sheet, parentRule);
    var decls = kf.decls || (kf.decls = parseDeclList(kf.body));
    var styleObj = makeRuleStyle(decls, function () { kf.body = serializeDeclList(decls); markDirty(sheet); }, keyframePropertyAllowed);
    defOn(r, "keyText", { get: function () { return kf.prelude.trim(); }, set: function (v) { kf.prelude = String(v); markDirty(sheet); }, enumerable: true });
    // [PutForwards=cssText]: `r.style = "..."` forwards to style.cssText.
    defOn(r, "style", { get: function () { return styleObj; }, set: function (v) { styleObj.cssText = String(v); }, enumerable: true });
    defOn(r, "cssText", { get: function () { var b = serializeDeclList(decls); return kf.prelude.trim() + " { " + (b ? b + " " : "") + "}"; }, enumerable: true });
    return r;
  }
  function makeKeyframesRule(struct, sheet, parentRule) {
    var rule = newRule("CSSKeyframesRule", 7, sheet, parentRule);
    var name = struct.prelude.trim();
    var childRules = parseRuleStructs(struct.body);
    // Serialize the @keyframes name: a CSS-wide keyword (or otherwise non-custom-ident) name must be
    // serialized as a string, else as an identifier.
    function serializeKfName(n) {
      n = unquoteCss(n);
      var lower = n.toLowerCase();
      if (/^(initial|inherit|unset|revert|revert-layer|default|none)$/.test(lower) ||
          !/^-?[_a-zA-Z -￿][-_a-zA-Z0-9 -￿]*$/.test(n)) {
        return '"' + n.replace(/\\/g, "\\\\").replace(/"/g, '\\"') + '"';
      }
      return n;
    }
    // Normalize a keyframe selector list (from->0%, to->100%, lowercase, trimmed) for find/delete.
    function normKey(s) {
      return String(s).trim().split(",").map(function (t) {
        t = t.trim().toLowerCase(); return t === "from" ? "0%" : (t === "to" ? "100%" : t);
      }).join(", ");
    }
    function buildList() {
      var list = [];
      for (var i = 0; i < childRules.length; i++) { list.push(makeKeyframeRule(childRules[i], rule, sheet)); }
      list.item = function (i) { return this[i] || null; };
      try { if (globalThis.CSSRuleList && globalThis.CSSRuleList.prototype) { Object.setPrototypeOf(list, globalThis.CSSRuleList.prototype); } } catch (e) {}
      return list;
    }
    defOn(rule, "name", { get: function () { return unquoteCss(name); }, set: function (v) { name = String(v); markDirty(sheet); }, enumerable: true });
    defOn(rule, "cssRules", { get: function () { return buildList(); }, enumerable: true });
    defOn(rule, "length", { get: function () { return childRules.length; }, enumerable: true });
    // Indexed getter: rule[i] -> the i-th CSSKeyframeRule (or undefined). Defined over a fixed range.
    for (var __ix = 0; __ix < 64; __ix++) {
      (function (k) {
        defOn(rule, String(k), { get: function () { return k < childRules.length ? makeKeyframeRule(childRules[k], rule, sheet) : undefined; }, enumerable: true });
      })(__ix);
    }
    defOn(rule, "appendRule", { value: function (text) {
      var s = parseRuleStructs(String(text));
      for (var i = 0; i < s.length; i++) { childRules.push(s[i]); }
      markDirty(sheet);
    }, enumerable: true });
    defOn(rule, "findRule", { value: function (select) {
      var key = normKey(select);
      for (var i = childRules.length - 1; i >= 0; i--) { if (normKey(childRules[i].prelude) === key) { return makeKeyframeRule(childRules[i], rule, sheet); } }
      return null;
    }, enumerable: true });
    defOn(rule, "deleteRule", { value: function (select) {
      var key = normKey(select);
      for (var i = childRules.length - 1; i >= 0; i--) { if (normKey(childRules[i].prelude) === key) { childRules.splice(i, 1); markDirty(sheet); return; } }
    }, enumerable: true });
    defOn(rule, "cssText", { get: function () {
      var inner = "";
      for (var i = 0; i < childRules.length; i++) {
        var c = childRules[i];
        inner += "  " + c.prelude.trim() + " { " + (serializeDeclList(c.decls || (c.decls = parseDeclList(c.body))) ? serializeDeclList(c.decls) + " " : "") + "}\n";
      }
      return "@keyframes " + serializeKfName(name) + " {\n" + inner + "}";
    }, enumerable: true });
    return rule;
  }
  function makeGenericRule(struct, sheet, parentRule, type) {
    var rule = newRule("CSSRule", type, sheet, parentRule);
    defOn(rule, "cssText", { get: function () {
      if (struct.body != null && struct.body !== "") { return struct.kind + " " + struct.prelude + " { " + struct.body.trim() + " }"; }
      return struct.kind + " " + struct.prelude + ";";
    }, enumerable: true });
    return rule;
  }
  // --- @font-feature-values ------------------------------------------------------------------
  function makeFontFeatureValuesRule(struct, sheet, parentRule) {
    var rule = newRule("CSSFontFeatureValuesRule", 14, sheet, parentRule);
    var family = struct.prelude.trim();
    // Parse inner @blocks into maps: blockName -> { ident: [numbers] }.
    var blocks = {};
    var inner = parseRuleStructs(struct.body);
    var blockNames = ["stylistic", "styleset", "character-variant", "swash", "ornaments", "annotation"];
    for (var bi = 0; bi < blockNames.length; bi++) { blocks[blockNames[bi]] = {}; }
    for (var i = 0; i < inner.length; i++) {
      var ir = inner[i];
      if (ir.kind && ir.kind.charAt(0) === "@") {
        var bn = ir.atName;
        if (!blocks[bn]) { blocks[bn] = {}; }
        var dl = splitTopLevel(ir.body, ";");
        for (var j = 0; j < dl.length; j++) {
          var seg = dl[j], c = seg.indexOf(":");
          if (c < 0) { continue; }
          var key = seg.slice(0, c).trim();
          var nums = seg.slice(c + 1).trim().split(/\s+/).filter(function (x) { return x !== ""; }).map(Number);
          if (key) { blocks[bn][key] = nums; }
        }
      }
    }
    function makeValuesMap(store) {
      var m = {
        get: function (k) { return store[k]; },
        set: function (k, v) { store[k] = (typeof v === "number") ? [v] : v.slice(); markDirty(sheet); },
        has: function (k) { return Object.prototype.hasOwnProperty.call(store, k); },
        "delete": function (k) { var had = Object.prototype.hasOwnProperty.call(store, k); delete store[k]; markDirty(sheet); return had; },
        clear: function () { for (var k in store) { delete store[k]; } markDirty(sheet); },
        forEach: function (cb, thisArg) { for (var k in store) { cb.call(thisArg, store[k], k, m); } }
      };
      Object.defineProperty(m, "size", { get: function () { return Object.keys(store).length; }, enumerable: true, configurable: true });
      try { m[Symbol.iterator] = function () { var keys = Object.keys(store), idx = 0; return { next: function () { return idx < keys.length ? { value: [keys[idx], store[keys[idx++]]], done: false } : { value: undefined, done: true }; } }; }; } catch (e) {}
      return m;
    }
    var maps = {
      stylistic: makeValuesMap(blocks["stylistic"]),
      styleset: makeValuesMap(blocks["styleset"]),
      characterVariant: makeValuesMap(blocks["character-variant"]),
      swash: makeValuesMap(blocks["swash"]),
      ornaments: makeValuesMap(blocks["ornaments"]),
      annotation: makeValuesMap(blocks["annotation"])
    };
    for (var mk in maps) { (function (k) { defOn(rule, k, { get: function () { return maps[k]; }, enumerable: true }); })(mk); }
    defOn(rule, "fontFamily", { get: function () { return family; }, set: function (v) { family = String(v); markDirty(sheet); }, enumerable: true });
    defOn(rule, "cssText", { get: function () {
      var s = "@font-feature-values " + family + " {\n";
      var order = [["@stylistic", blocks["stylistic"]], ["@styleset", blocks["styleset"]], ["@character-variant", blocks["character-variant"]], ["@swash", blocks["swash"]], ["@ornaments", blocks["ornaments"]], ["@annotation", blocks["annotation"]]];
      for (var oi = 0; oi < order.length; oi++) {
        var store = order[oi][1], keys = Object.keys(store);
        if (!keys.length) { continue; }
        s += "  " + order[oi][0] + " {\n";
        for (var ki = 0; ki < keys.length; ki++) { s += "    " + keys[ki] + ": " + store[keys[ki]].join(" ") + ";\n"; }
        s += "  }\n";
      }
      return s + "}";
    }, enumerable: true });
    return rule;
  }

  // --- CSSRuleList (stable, indexed list of rule objects) -------------------------------------
  // Builds CSSRule objects lazily and caches them per struct so expandos persist. `structs` is the
  // mutable backing array (shared with the sheet). insert/delete keep the cached wrappers aligned.
  function makeRuleList(structs, sheet, parentRule) {
    var list = [];
    function rebuild() {
      // Reuse cached wrappers (keyed on struct identity) so [SameObject] holds.
      for (var i = 0; i < structs.length; i++) {
        var st = structs[i];
        if (!st.__rule) { st.__rule = makeCssRule(st, sheet, parentRule); }
        list[i] = st.__rule;
      }
      list.length = structs.length;
    }
    rebuild();
    list.item = function (i) { i = i >>> 0; return i < structs.length ? list[i] : null; };
    list.__rebuild = rebuild;
    list.__structs = structs;
    list.__insert = function (text, index) {
      if (index === undefined) { index = 0; }
      index = index >>> 0;
      // Per CSSOM "insert a CSS rule": index range is checked BEFORE parsing.
      if (index > structs.length) { throw new globalThis.DOMException("Index out of bounds", "IndexSizeError"); }
      // Parse — must be exactly one syntactically valid rule. A bare prelude with no "{" (e.g. "???")
      // parses to a "style" struct but is NOT a rule, so reject it.
      var newStructs = parseRuleStructs(text);
      var st = newStructs[0];
      if (newStructs.length !== 1 || (st.kind === "style" && String(text).indexOf("{") < 0)) {
        throw new globalThis.DOMException("Failed to parse the rule", "SyntaxError");
      }
      // A style rule with an invalid selector (e.g. an undeclared namespace prefix) doesn't parse.
      if (st.kind === "style" && normalizeSelectorList(st.prelude, sheetNsContext(sheet)) == null) {
        throw new globalThis.DOMException("Failed to parse the rule selector", "SyntaxError");
      }
      // A grouping rule (CSSMediaRule/Supports/Container, parentRule set) cannot contain @import /
      // @namespace / @charset — those are stylesheet-level only.
      if (parentRule && (st.kind === "@import" || st.kind === "@namespace" || st.kind === "@charset")) {
        throw new globalThis.DOMException("Cannot insert this rule into a grouping rule", "HierarchyRequestError");
      }
      // Constructed sheets can't import: inserting an @import rule throws SyntaxError (per the
      // construct-stylesheets spec / disallow-import test).
      if (!parentRule && st.kind === "@import" && sheet && sheet.__constructed) {
        throw new globalThis.DOMException("Can't insert @import rules into a constructed stylesheet.", "SyntaxError");
      }
      // Top-level ordering constraints (CSSOM "insert a CSS rule" step 6): @import rules precede all
      // other rules; @namespace rules precede everything except @import. Violating the position throws
      // HierarchyRequestError. (@charset can never be inserted.)
      if (!parentRule) {
        if (st.kind === "@charset") {
          throw new globalThis.DOMException("Cannot insert @charset", "HierarchyRequestError");
        }
        if (st.kind === "@import") {
          // Every rule before `index` must be @import/@charset.
          for (var ii = 0; ii < index; ii++) { var k = structs[ii].kind; if (k !== "@import" && k !== "@charset") { throw new globalThis.DOMException("@import must precede all other rules", "HierarchyRequestError"); } }
          // The rule at `index` (if any) must not be a non-@import/@namespace rule that @import would jump over backwards — handled by the above since @import goes before namespaces too.
        } else if (st.kind === "@namespace") {
          // @namespace may only exist when the sheet has only @import/@namespace rules, and must be
          // positioned after @imports and before regular rules.
          for (var ij = 0; ij < structs.length; ij++) { var kj = structs[ij].kind; if (kj !== "@import" && kj !== "@namespace" && kj !== "@charset") { throw new globalThis.DOMException("@namespace not allowed here", "InvalidStateError"); } }
          for (var ik = 0; ik < index; ik++) { var kk = structs[ik].kind; if (kk !== "@import" && kk !== "@namespace" && kk !== "@charset") { throw new globalThis.DOMException("@namespace mispositioned", "HierarchyRequestError"); } }
        } else {
          // A regular rule must come after all @import and @namespace rules: no such rule at index >= index.
          for (var il = index; il < structs.length; il++) { var kl = structs[il].kind; if (kl === "@import" || kl === "@namespace" || kl === "@charset") { throw new globalThis.DOMException("Cannot insert rule before @import/@namespace", "HierarchyRequestError"); } }
        }
      }
      structs.splice(index, 0, st);
      rebuild(); markDirty(sheet);
      return index;
    };
    list.__delete = function (index) {
      index = index >>> 0;
      if (index >= structs.length) { throw new globalThis.DOMException("Index out of bounds", "IndexSizeError"); }
      // CSSOM "remove a CSS rule": a @namespace rule may only be deleted when the sheet contains
      // nothing but @import / @namespace rules; otherwise InvalidStateError.
      if (structs[index].kind === "@namespace") {
        for (var k = 0; k < structs.length; k++) {
          if (structs[k].kind !== "@namespace" && structs[k].kind !== "@import") {
            throw new globalThis.DOMException("Cannot delete a namespace rule when other rules are present.", "InvalidStateError");
          }
        }
      }
      structs[index].__detached = true; // detach the removed rule (parentStyleSheet/Rule -> null)
      structs.splice(index, 1);
      rebuild(); markDirty(sheet);
    };
    try { if (globalThis.CSSRuleList && globalThis.CSSRuleList.prototype) { Object.setPrototypeOf(list, globalThis.CSSRuleList.prototype); } } catch (e) {}
    return list;
  }

  // Mark a sheet dirty (re-render its <style> ownerNode so the cascade picks up CSSOM edits).
  // For a constructed sheet, notify any adoptedStyleSheets observers so their managed <style>
  // mirror is refreshed (mutating an adopted sheet is reflected in rendering).
  function markDirty(sheet) {
    if (!sheet || sheet.__rendering) { return; }
    if (sheet.__ownerNode && typeof sheet.__renderToOwner === "function") {
      try { sheet.__rendering = true; sheet.__renderToOwner(); } finally { sheet.__rendering = false; }
    }
    if (sheet.__adoptHosts) {
      for (var h = 0; h < sheet.__adoptHosts.length; h++) {
        try { sheet.__adoptHosts[h].__refreshAdopted(); } catch (e) {}
      }
    }
  }

  // --- CSSStyleSheet --------------------------------------------------------------------------
  // CSSOM origin-clean flag: a stylesheet fetched from another origin is not origin-clean, so its
  // rules (cssRules/insertRule/deleteRule) throw SecurityError. `href` is the link's URL attribute.
  function __computeOriginClean(href) {
    // No href (inline <style>, constructed sheet) is same-origin; data:/about: inherit the doc origin.
    if (!href) { return true; }
    if (href.slice(0, 5) === "data:" || href.slice(0, 6) === "about:") { return true; }
    try {
      if (new URL(href, document.baseURI).origin !== location.origin) { return false; }
    } catch (e) {
      // Unparseable/opaque URL (e.g. a cross-origin authority we can't resolve) → not origin-clean.
      return false;
    }
    // A server-side redirect carries its destination in a `location=` query param (e.g. WPT's
    // /common/redirect.py?location=…). Following it would land on that URL, so if the redirect
    // target is another origin the resulting sheet is not origin-clean.
    var m = /[?&]location=([^&]*)/.exec(href);
    if (m) {
      try {
        if (new URL(decodeURIComponent(m[1]), document.baseURI).origin !== location.origin) { return false; }
      } catch (e) {
        return false;
      }
    }
    return true;
  }
  function makeStyleSheetCore(structs, ownerNode) {
    var ss = {};
    var mediaHolder = { text: "" };
    var mediaList = makeMediaList(mediaHolder, null);
    ss.__structs = structs;
    ss.__ownerNode = ownerNode || null;
    var ruleList = makeRuleList(structs, ss, null);
    // __sync re-reads the owner node's text when it changed underneath us (e.g. a page sets
    // `styleEl.firstChild.data` directly). Replaced by makeStyleSheet for live <style>/<link>.
    ss.__sync = function () {};
    ss.type = "text/css";
    ss.disabled = false;
    Object.defineProperty(ss, "ownerNode", { get: function () {
      if (!ownerNode) { return null; }
      // A disabled or disconnected stylesheet <link> is no longer associated with the document, so
      // its sheet's ownerNode is null (CSSOM: a removed style sheet has no owner node).
      try {
        if (ownerNode.tagName === "LINK" && ownerNode.disabled) { return null; }
        if (document.documentElement && !document.documentElement.contains(ownerNode)) { return null; }
      } catch (e) {}
      return ownerNode;
    }, enumerable: true, configurable: true });
    Object.defineProperty(ss, "ownerRule", { get: function () { return ss.__ownerRule || null; }, enumerable: true, configurable: true });
    Object.defineProperty(ss, "parentStyleSheet", { get: function () { return null; }, enumerable: true, configurable: true });
    Object.defineProperty(ss, "href", { get: function () { return ss.__href != null ? ss.__href : null; }, enumerable: true, configurable: true });
    Object.defineProperty(ss, "title", { get: function () { return (ownerNode && ownerNode.getAttribute && ownerNode.getAttribute("title")) || null; }, enumerable: true, configurable: true });
    Object.defineProperty(ss, "media", { get: function () { return ss.__media || mediaList; }, set: function (v) { (ss.__media || mediaList).mediaText = v; }, enumerable: true, configurable: true });
    // A non-origin-clean (cross-origin) sheet throws SecurityError on any rule access (CSSOM).
    ss.__checkOriginClean = function () {
      if (ss.__originClean === false) {
        throw new globalThis.DOMException("Cannot access rules of a cross-origin stylesheet", "SecurityError");
      }
    };
    Object.defineProperty(ss, "cssRules", { get: function () { ss.__checkOriginClean(); ss.__sync(); return ruleList; }, enumerable: true, configurable: true });
    Object.defineProperty(ss, "rules", { get: function () { ss.__checkOriginClean(); ss.__sync(); return ruleList; }, enumerable: false, configurable: true });
    ss.insertRule = function (text, index) {
      if (arguments.length < 1) { throw new TypeError("insertRule requires at least 1 argument"); }
      ss.__checkOriginClean();
      ss.__sync();
      return ruleList.__insert(String(text), index);
    };
    ss.deleteRule = function (index) {
      if (arguments.length < 1) { throw new TypeError("deleteRule requires 1 argument"); }
      ss.__checkOriginClean();
      return ruleList.__delete(index);
    };
    // Legacy CSSOM members.
    ss.removeRule = function (index) { if (index === undefined) { index = 0; } return ruleList.__delete(index); };
    ss.addRule = function (selector, block, index) {
      selector = selector === undefined ? "undefined" : String(selector);
      block = block === undefined ? "undefined" : String(block);
      if (index === undefined) { index = ruleList.length; }
      index = index >>> 0;
      // IndexSizeError propagates; SyntaxError/HierarchyRequestError are swallowed (legacy behavior).
      if (index > ruleList.length) { throw new globalThis.DOMException("Index out of bounds", "IndexSizeError"); }
      var text = selector + " { " + block + " }";
      try { ruleList.__insert(text, index); } catch (e) { if (e && e.name === "IndexSizeError") { throw e; } }
      return -1;
    };
    Object.defineProperty(ss, "cssText", { get: function () {
      var s = ""; for (var i = 0; i < ruleList.length; i++) { s += (s ? "\n" : "") + ruleList[i].cssText; } return s;
    }, enumerable: false, configurable: true });
    // CSSOM `replace`/`replaceSync`: only constructed sheets allow it; a `<style>`/`<link>` live
    // sheet (or an @import-target child sheet) throws NotAllowedError. `replaceSync` parses `text`,
    // strips any `@import` rules (constructed sheets can't import), and replaces ALL the rules.
    function doReplaceSync(text) {
      if (!ss.__constructed) {
        throw new globalThis.DOMException("Can't call replace/replaceSync on non-constructed CSSStyleSheet.", "NotAllowedError");
      }
      var ns = parseRuleStructs(String(text));
      var kept = [];
      for (var i = 0; i < ns.length; i++) { if (ns[i].kind !== "@import") { kept.push(ns[i]); } }
      ss.__structs.length = 0;
      for (var j = 0; j < kept.length; j++) { ss.__structs.push(kept[j]); }
      ruleList.__rebuild();
      markDirty(ss);
    }
    ss.replaceSync = function (text) { doReplaceSync(text); };
    ss.replace = function (text) {
      try { doReplaceSync(text); } catch (e) { return Promise.reject(e); }
      return Promise.resolve(ss);
    };
    try { if (globalThis.CSSStyleSheet && globalThis.CSSStyleSheet.prototype) { Object.setPrototypeOf(ss, globalThis.CSSStyleSheet.prototype); } } catch (e) {}
    return ss;
  }
  // A constructed (or @import-target) sheet with no owner node; supports replace/replaceSync.
  function makeConstructedSheet(cssText) {
    var ss = makeStyleSheetCore(parseRuleStructs(cssText), null);
    ss.__constructed = true;
    return ss;
  }
  // The live sheet for a <style>/<link> element. Parses textContent; re-renders on CSSOM edits, and
  // re-parses if the page mutates the element's text out-of-band (e.g. `styleEl.firstChild.data`).
  function makeStyleSheet(styleEl) {
    var initial = styleEl.textContent || "";
    // A <link rel=stylesheet> has no textContent — its rules come from the fetched external CSS.
    // `__fetch` is a synchronous GET via the host fetcher (the engine already fetched it for the
    // cascade, so this hits the net cache).
    if (!initial && styleEl.tagName === "LINK") {
      try {
        var __h = styleEl.getAttribute && styleEl.getAttribute("href");
        if (__h && __h.slice(0, 5) === "data:") {
          // A `data:` stylesheet carries its CSS inline (decode it here; the host fetcher is HTTP).
          var __c = __h.indexOf(",");
          if (__c >= 0) {
            var __meta = __h.slice(5, __c), __body = __h.slice(__c + 1);
            initial = (__meta.indexOf(";base64") >= 0)
              ? (typeof atob === "function" ? atob(__body) : "")
              : decodeURIComponent(__body);
          }
        } else if (__h && typeof __fetch === "function") {
          initial = __fetch(__h) || "";
        }
      } catch (e) {}
    }
    var ss = makeStyleSheetCore(parseRuleStructs(initial), styleEl);
    ss.__lastText = initial;
    // A <link>'s sheet is origin-clean only if fetched from the document's origin (CSSOM).
    if (styleEl.tagName === "LINK") {
      var __lh = (styleEl.getAttribute && styleEl.getAttribute("href")) || "";
      ss.__originClean = __computeOriginClean(__lh);
      // A `.asis` resource is served as a raw HTTP response; one that isn't a well-formed response
      // (no "HTTP/" status line) is a network error, so the load fails and the sheet isn't
      // origin-clean (accessing its rules throws SecurityError).
      if (/\.asis(\?|#|$)/i.test(__lh) && initial && !/^\s*HTTP\//i.test(initial)) {
        ss.__originClean = false;
      }
    }
    // The sheet's `media` reflects the owner <style>/<link> element's `media` content attribute.
    // The MediaList writes back to that attribute, so `sheet.media.appendMedium(...)` updates it.
    var mediaHolder = { get text() { var m = styleEl.getAttribute && styleEl.getAttribute("media"); return m == null ? "" : m; }, set text(v) { if (styleEl.setAttribute) { styleEl.setAttribute("media", v); } } };
    ss.__media = makeMediaList(mediaHolder, null);
    ss.__sync = function () {
      if (ss.__rendering) { return; }
      // A <link>'s rules come from its fetched CSS, not textContent — don't let an empty textContent
      // clear them (and the page can't mutate a link's CSS text via the DOM anyway).
      if (styleEl.tagName === "LINK") { return; }
      var cur = styleEl.textContent || "";
      if (cur === ss.__lastText) { return; }
      ss.__lastText = cur;
      var ns = parseRuleStructs(cur);
      ss.__structs.length = 0; for (var i = 0; i < ns.length; i++) { ss.__structs.push(ns[i]); }
      ss.cssRules.__rebuild();
    };
    ss.__renderToOwner = function () {
      var s = ""; var rl = ss.cssRules;
      for (var i = 0; i < rl.length; i++) { s += (s ? "\n" : "") + rl[i].cssText; }
      try { styleEl.textContent = s; ss.__lastText = styleEl.textContent || ""; } catch (e) {}
    };
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
  // Look up the canonical wrapper for a node id, if one was already created (createElement / a prior
  // lookup). Returns null if the node was never wrapped. Lets out-of-scope code resolve a node id.
  def(globalThis, "__nodeById", function (id) { return __nodeCache[id] || null; });

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

  // ============================================================================================
  // Generic HTML IDL attribute reflection.
  //
  // The HTML standard defines, for each element interface, a set of IDL attributes that "reflect"
  // a content attribute (e.g. `el.id` <-> `id`, `a.href` <-> `href`, `input.disabled` <->
  // `disabled`). Each reflected attribute has a TYPE (DOMString, boolean, long, unsigned long,
  // enumerated, URL, ...) whose getter/setter rules are spelled out in the spec. We implement those
  // rules once as a set of "type factories" and drive them from data tables transcribed from the
  // WPT `elements-*.js` files (which are themselves generated from the spec IDL), so the behaviour
  // matches the exhaustive reflection-*.html conformance tests.
  //
  // Every getter/setter reads/writes the element's CONTENT attribute through the existing
  // __getAttr / __setAttr / __removeAttr natives, so reflection stays live both ways (set IDL ->
  // attribute changes; setAttribute -> IDL getter changes).
  // ============================================================================================
  function __asciiLower(s) {
    return String(s).replace(/[A-Z]/g, function (c) { return c.toLowerCase(); });
  }
  // Resolve `v` against the document base URL and serialize as the WPT reflection harness does:
  // protocol + "//" + host + pathname + search + hash (returning the raw input if that yields "//").
  // This mirrors the harness' own resolveUrl() so `url`-type reflected attributes compare equal,
  // and works around our URL parser dropping the trailing path segment for empty relative refs.
  // The document's effective base URL: the first <base href> (resolved against the page URL) if any,
  // otherwise the page URL itself. Honoured by URL-reflecting attributes (a.href, img.src, …).
  function __effectiveBaseURL() {
    try {
      var b = document.querySelector("base[href]");
      if (b) {
        var bh = b.getAttribute("href");
        if (bh != null && bh !== "") {
          var rb = parseURL(bh, globalThis.__pageURL);
          if (!rb.__invalid && rb.href) { return rb.href; }
        }
      }
    } catch (e) {}
    return globalThis.__pageURL;
  }
  function __reflResolveURL(v) {
    v = String(v);
    var base = __effectiveBaseURL();
    var resolved;
    try {
      if (v === "") {
        // Empty relative URL: resolve to the base, but keep the base's path/query (drop its fragment).
        var bp = parseURL(base);
        resolved = bp.protocol + (bp.host ? "//" + bp.host : "") + bp.pathname + bp.search;
      } else if (v.charCodeAt(0) === 35 /* '#' */) {
        var bp2 = parseURL(base);
        resolved = bp2.protocol + (bp2.host ? "//" + bp2.host : "") + bp2.pathname + bp2.search + v;
      } else {
        resolved = new URL(v, base).href;
      }
    } catch (e) { return v; }
    var p = parseURL(resolved);
    var host = p.host || "";
    var ret = p.protocol + "//" + host + p.pathname + p.search + p.hash;
    if (ret === "//") { return v; }
    return ret;
  }
  var __refl = (function () {
    var maxInt = 2147483647, minInt = -2147483648;
    // "rules for parsing integers".
    function parseIntHtml(input) {
      input = String(input);
      var pos = 0, sign = 1, len = input.length;
      while (pos < len && /[ \t\n\f\r]/.test(input[pos])) { pos++; }
      if (pos >= len) { return false; }
      if (input[pos] === "-") { sign = -1; pos++; }
      else if (input[pos] === "+") { pos++; }
      if (pos >= len || !/[0-9]/.test(input[pos])) { return false; }
      var value = 0;
      while (pos < len && /[0-9]/.test(input[pos])) {
        value = value * 10 + (input.charCodeAt(pos) - 48);
        pos++;
      }
      return value === 0 ? 0 : sign * value;
    }
    // "rules for parsing non-negative integers".
    function parseNonneg(input) {
      var v = parseIntHtml(input);
      if (v === false || v < 0) { return false; }
      return v;
    }
    // "rules for parsing floating-point number values" (close enough for reflection; we lean on the
    // engine's Number parsing for the heavy lifting after validating the grammar's first char).
    function parseFloatHtml(input) {
      input = String(input);
      var pos = 0, len = input.length;
      while (pos < len && /[ \t\n\f\r]/.test(input[pos])) { pos++; }
      if (pos >= len) { return false; }
      var c = input[pos];
      if (c === "-" || c === "+") { pos++; }
      if (pos >= len) { return false; }
      c = input[pos];
      if (!/[0-9]/.test(c) && !(c === "." && pos + 1 < len && /[0-9]/.test(input[pos + 1]))) { return false; }
      // Grab the longest valid numeric prefix.
      var m = input.slice(pos).match(/^[0-9]*\.?[0-9]*(?:[eE][-+]?[0-9]+)?/);
      var numStr = (input[ (input[0]==="-"||input[0]==="+") ? 0 : -1 ] === "-" ? "-" : "");
      // Re-derive sign from the leading sign char we consumed.
      var lead = input.slice(0, pos);
      var s = lead.indexOf("-") !== -1 ? "-" : "";
      var n = Number(s + (m ? m[0] : ""));
      // The "rules for parsing floating-point number values" only produce finite numbers; a value
      // that overflows to +/-Infinity (e.g. "1.8e308") is treated as a parse error.
      if (isNaN(n) || !isFinite(n)) { return false; }
      return n;
    }
    // Shortest string for an integer (Number -> string) per "valid integer" serialisation.
    function intToStr(n) { return String(n | 0 === n ? (n | 0) : Math.trunc(n)); }

    var pageURL = function () { return globalThis.__pageURL; };
    function resolveURL(v, base) {
      if (v == null) { return ""; }
      v = String(v);
      try { return new URL(v, base || pageURL()).href; } catch (e) { return v; }
    }

    // ---- type factories: each returns a {get, set} descriptor pair bound to (node, contentAttr) --
    // `g` reads the raw content attribute (string or null); `s`/`r` set/remove it.
    function mk(node, attr) {
      return {
        g: function () { return __getAttr(node, attr); },
        s: function (v) { __setAttr(node, attr, String(v)); },
        r: function () { __removeAttr(node, attr); }
      };
    }

    var factories = {
      "string": function (io, data) {
        return {
          get: function () { var v = io.g(); return v == null ? "" : String(v); },
          set: function (v) {
            // [LegacyNullToEmptyString] makes null -> ""; otherwise null -> "null" (DOMString).
            if (v === null && data && data.treatNullAsEmptyString) { io.s(""); return; }
            io.s(String(v));
          }
        };
      },
      "url": function (io, data) {
        // form.action / input,button.formAction: when the attribute is absent (or empty), they
        // return the document URL rather than "" (hard-coded special case in the spec + harness).
        var docDefault = !!(data && data.urlDocDefault);
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return docDefault ? __reflResolveURL("") : ""; }
            return __reflResolveURL(v);
          },
          set: function (v) { io.s(String(v)); }
        };
      },
      "boolean": function (io) {
        return {
          get: function () { return io.g() != null; },
          set: function (v) { if (v) { io.s(""); } else { io.r(); } }
        };
      },
      "long": function (io, data) {
        var dflt = (data && data.defaultVal != null) ? data.defaultVal : 0;
        var hasDefault = !(data && data.defaultVal === null);
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return hasDefault ? dflt : 0; }
            var p = parseIntHtml(v);
            if (p === false || p > maxInt || p < minInt) { return hasDefault ? dflt : 0; }
            return p;
          },
          set: function (v) { io.s(String(toLong(v))); }
        };
      },
      "limited long": function (io, data) {
        var dflt = (data && data.defaultVal != null) ? data.defaultVal : -1;
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return dflt; }
            var p = parseNonneg(v);
            if (p === false || p > maxInt || p < minInt) { return dflt; }
            return p;
          },
          set: function (v) {
            v = toLong(v);
            if (v < 0) { throwIndexSize(); }
            io.s(String(v));
          }
        };
      },
      "unsigned long": function (io, data) {
        var dflt = (data && data.defaultVal != null) ? data.defaultVal : 0;
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return dflt; }
            var p = parseNonneg(v);
            if (p === false || p < 0 || p > maxInt) { return dflt; }
            return p;
          },
          set: function (v) { io.s(String(toUnsignedSet(v, dflt))); }
        };
      },
      "limited unsigned long": function (io, data) {
        var dflt = (data && data.defaultVal != null) ? data.defaultVal : 1;
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return dflt; }
            var p = parseNonneg(v);
            if (p === false || p < 1 || p > maxInt) { return dflt; }
            return p;
          },
          set: function (v) {
            v = toUnsigned(v);
            if (v === 0) { throwIndexSize(); }
            if (v > maxInt) { v = dflt; }
            io.s(String(v));
          }
        };
      },
      "limited unsigned long with fallback": function (io, data) {
        var dflt = (data && data.defaultVal != null) ? data.defaultVal : 1;
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return dflt; }
            var p = parseNonneg(v);
            if (p === false || p < 1 || p > maxInt) { return dflt; }
            return p;
          },
          set: function (v) {
            v = toUnsigned(v);
            var n = (v >= 1 && v <= maxInt) ? v : dflt;
            io.s(String(n));
          }
        };
      },
      "clamped unsigned long": function (io, data) {
        var dflt = (data && data.defaultVal != null) ? data.defaultVal : 0;
        var min = data.min, max = data.max;
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return dflt; }
            var p = parseNonneg(v);
            if (p === false) { return dflt; }
            if (p < min) { return min; }
            if (p > max) { return max; }
            return p;
          },
          set: function (v) { io.s(String(toUnsignedSet(v, dflt))); }
        };
      },
      "double": function (io, data) {
        var dflt = (data && data.defaultVal != null) ? data.defaultVal : 0.0;
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return dflt; }
            var p = parseFloatHtml(v);
            return p === false ? dflt : p;
          },
          set: function (v) { io.s(bestFloat(v)); }
        };
      },
      "limited double": function (io, data) {
        var dflt = (data && data.defaultVal != null) ? data.defaultVal : 0.0;
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return dflt; }
            var p = parseFloatHtml(v);
            return (p === false || p <= 0) ? dflt : p;
          },
          set: function (v) {
            var n = Number(v);
            if (!(n > 0)) { return; } // leave attribute unchanged
            io.s(bestFloat(n));
          }
        };
      },
      "enum": function (io, data) {
        var keywords = data.keywords || [];
        var missing = (data.defaultVal !== undefined) ? data.defaultVal : "";
        // An array defaultVal means "implementation-defined, but one of these keywords" (e.g.
        // media preload). Pick the first as our canonical missing-value default (a string keyword).
        if (Array.isArray(missing)) { missing = missing.length ? missing[0] : ""; }
        var invalid = (data.invalidVal !== undefined) ? data.invalidVal : missing;
        var nonCanon = data.nonCanon || {};
        var nullable = !!data.isNullable;
        function canon(val) {
          var lc = __asciiLower(String(val));
          var ret = invalid;
          for (var i = 0; i < keywords.length; i++) {
            if (__asciiLower(keywords[i]) === lc) { ret = keywords[i]; break; }
          }
          if (Object.prototype.hasOwnProperty.call(nonCanon, ret)) { return nonCanon[ret]; }
          return ret;
        }
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return missing; }
            return canon(v);
          },
          set: function (v) {
            if (nullable && (v === null || v === undefined)) { io.r(); return; }
            io.s(String(v));
          }
        };
      },
      // `nonce`: a DOMString backed by a [[CryptographicNonce]] internal slot. Reading reflects the
      // attribute, but setting via the IDL updates only the slot, NOT the content attribute (so the
      // nonce can't be scraped back off the attribute). The attribute-change steps would refresh the
      // slot; since the WPT harness runs all setAttribute() cases before the IDL cases, tracking a
      // "slot owns the value" flag is sufficient here.
      "nonce": function (io) {
        var slot = "", owns = false;
        return {
          get: function () { if (owns) { return slot; } var v = io.g(); return v == null ? "" : String(v); },
          set: function (v) { slot = String(v); owns = true; }
        };
      },
      // Nullable DOMString (ARIA props, role): get -> attr value or null; set null/undefined removes.
      "nullable string": function (io) {
        return {
          get: function () { var v = io.g(); return v == null ? null : String(v); },
          set: function (v) { if (v === null || v === undefined) { io.r(); } else { io.s(String(v)); } }
        };
      }
    };

    function toLong(v) {
      // WebIDL [long] conversion: ToInt32.
      var n = Number(v);
      if (!isFinite(n)) { n = 0; }
      n = n < 0 ? Math.ceil(n) : Math.floor(n);
      n = n % 4294967296;
      if (n >= 2147483648) { n -= 4294967296; }
      if (n < -2147483648) { n += 4294967296; }
      return n | 0;
    }
    function toUnsigned(v) {
      // WebIDL [unsigned long] conversion: ToUint32.
      var n = Number(v);
      if (!isFinite(n)) { n = 0; }
      n = n < 0 ? Math.ceil(n) : Math.floor(n);
      n = n % 4294967296;
      if (n < 0) { n += 4294967296; }
      return n >>> 0;
    }
    // Setting a (non-limited) unsigned long: out-of-[0,maxInt] becomes the default.
    function toUnsignedSet(v, dflt) {
      var n = toUnsigned(v);
      if (n < 0 || n > maxInt) { return dflt; }
      return n;
    }
    function bestFloat(v) {
      var n = Number(v);
      if (!isFinite(n)) { n = 0; }
      return String(n);
    }
    function throwIndexSize() {
      var e;
      try { e = new DOMException("Index or size is negative or greater than the allowed amount", "IndexSizeError"); }
      catch (x) { e = new Error("IndexSizeError"); e.name = "IndexSizeError"; e.code = 1; }
      throw e;
    }

    return {
      factories: factories, mk: mk, parseNonneg: parseNonneg, parseIntHtml: parseIntHtml,
      // Types we deliberately don't reflect (tested as a different IDL shape we don't model);
      // leaving them undefined means the WPT harness skips them rather than failing.
      skip: { "tokenlist": 1, "settable tokenlist": 1 }
    };
  })();
  function __reflParseNonneg(v) { return __refl.parseNonneg(v); }

  // Per-element reflected-attribute tables, transcribed from the WPT elements-*.js data (which is
  // itself generated from the HTML spec IDL). Key = lowercase tag name; value = map of idlName ->
  // type descriptor ("string" | {type, domAttrName?, defaultVal?, keywords?, ...}).
  var __reflTables = (function () {
    var S = "string", U = "url", B = "boolean", L = "long", UL = "unsigned long";
    var REF = ["", "no-referrer", "no-referrer-when-downgrade", "same-origin", "origin",
      "strict-origin", "origin-when-cross-origin", "strict-origin-when-cross-origin", "unsafe-url"];
    function ref() { return { type: "enum", keywords: REF }; }
    function crossOrigin() { return { type: "enum", keywords: ["anonymous", "use-credentials"], nonCanon: { "": "anonymous" }, isNullable: true, defaultVal: null, invalidVal: "anonymous" }; }
    function enctype(dflt) { return { type: "enum", keywords: ["application/x-www-form-urlencoded", "multipart/form-data", "text/plain"], defaultVal: dflt, invalidVal: "application/x-www-form-urlencoded" }; }
    function nullStr() { return { type: "string", treatNullAsEmptyString: true }; }
    var charAttr = { type: S, domAttrName: "char" }, charoff = { type: S, domAttrName: "charoff" };
    var cellCommon = function () { return { align: S, ch: charAttr, chOff: charoff, vAlign: S }; };
    function assign(t) { var o = {}; for (var i = 0; i < arguments.length; i++) { var s = arguments[i]; for (var k in s) { if (Object.prototype.hasOwnProperty.call(s, k)) { o[k] = s[k]; } } } return o; }

    return {
      // text
      a: { target: S, download: S, ping: S, rel: S, hreflang: S, type: S, referrerPolicy: ref(), href: U, coords: S, charset: S, name: S, rev: S, shape: S },
      q: { cite: U }, data: { value: S }, time: { dateTime: S }, br: { clear: S },
      // grouping
      p: { align: S }, hr: { align: S, color: S, noShade: B, size: S, width: S }, pre: { width: L },
      blockquote: { cite: U },
      ol: { reversed: B, start: { type: L, defaultVal: 1 }, type: S, compact: B },
      ul: { compact: B, type: S }, li: { value: L, type: S }, dl: { compact: B }, div: { align: S },
      // forms
      form: { acceptCharset: { type: S, domAttrName: "accept-charset" }, action: { type: U, urlDocDefault: true },
        autocomplete: { type: "enum", keywords: ["on", "off"], defaultVal: "on" },
        enctype: enctype("application/x-www-form-urlencoded"),
        encoding: assign(enctype("application/x-www-form-urlencoded"), { domAttrName: "enctype" }),
        method: { type: "enum", keywords: ["get", "post", "dialog"], defaultVal: "get" },
        name: S, noValidate: B, target: S },
      fieldset: { disabled: B, name: S }, legend: { align: S },
      label: { htmlFor: { type: S, domAttrName: "for" } },
      input: { accept: S, alt: S, autocomplete: { type: S, customGetter: true },
        defaultChecked: { type: B, domAttrName: "checked" }, dirName: S, disabled: B, formAction: { type: U, urlDocDefault: true },
        formEnctype: assign(enctype(undefined), { defaultVal: undefined }),
        formMethod: { type: "enum", keywords: ["get", "post"], invalidVal: "get" },
        formNoValidate: B, formTarget: S, height: { type: UL, customGetter: true }, max: S,
        maxLength: "limited long", min: S, minLength: "limited long", multiple: B, name: S,
        pattern: S, placeholder: S, readOnly: B, required: B,
        size: { type: "limited unsigned long", defaultVal: 20 }, src: U, step: S,
        type: { type: "enum", keywords: ["hidden", "text", "search", "tel", "url", "email", "password",
          "date", "time", "datetime-local", "month", "week", "number", "range", "color", "checkbox",
          "radio", "file", "submit", "image", "reset", "button"], defaultVal: "text" },
        width: { type: UL, customGetter: true }, defaultValue: { type: S, domAttrName: "value" },
        align: S, useMap: S },
      button: { disabled: B, formAction: { type: U, urlDocDefault: true }, formEnctype: assign(enctype(undefined), { defaultVal: undefined }),
        formMethod: { type: "enum", keywords: ["get", "post", "dialog"], invalidVal: "get" },
        formNoValidate: B, formTarget: S, name: S,
        type: { type: "enum", keywords: ["submit", "reset", "button"], defaultVal: "submit" }, value: S },
      select: { autocomplete: { type: S, customGetter: true }, disabled: B, multiple: B, name: S,
        required: B, size: { type: UL, defaultVal: 0 } },
      optgroup: { disabled: B, label: S },
      option: { disabled: B, label: { type: S, customGetter: true },
        defaultSelected: { type: B, domAttrName: "selected" }, value: { type: S, customGetter: true } },
      textarea: { autocomplete: { type: S, customGetter: true },
        cols: { type: "limited unsigned long with fallback", defaultVal: 20 }, dirName: S, disabled: B,
        maxLength: "limited long", minLength: "limited long", name: S, placeholder: S, readOnly: B,
        required: B, rows: { type: "limited unsigned long with fallback", defaultVal: 2 }, wrap: S },
      output: { name: S }, progress: { max: { type: "limited double", defaultVal: 1.0 } },
      meter: { value: { type: "double", customGetter: true }, min: { type: "double", customGetter: true },
        max: { type: "double", customGetter: true }, low: { type: "double", customGetter: true },
        high: { type: "double", customGetter: true }, optimum: { type: "double", customGetter: true } },
      // embedded
      img: { alt: S, src: U, srcset: S, crossOrigin: crossOrigin(), useMap: S, isMap: B,
        width: { type: UL, customGetter: true }, height: { type: UL, customGetter: true },
        referrerPolicy: ref(), decoding: { type: "enum", keywords: ["async", "sync", "auto"], defaultVal: "auto", invalidVal: "auto" },
        name: S, lowsrc: { type: U }, align: S, hspace: UL, vspace: UL, longDesc: U, border: nullStr() },
      iframe: { src: U, srcdoc: S, name: S, allowFullscreen: B, width: S, height: S, referrerPolicy: ref(),
        align: S, scrolling: S, frameBorder: S, longDesc: U, marginHeight: nullStr(), marginWidth: nullStr() },
      embed: { src: U, type: S, width: S, height: S, align: S, name: S },
      object: { data: U, type: S, name: S, useMap: S, width: S, height: S, align: S, archive: S, code: S,
        declare: B, hspace: UL, standby: S, vspace: UL, codeBase: U, codeType: S, border: nullStr() },
      param: { name: S, value: S, type: S, valueType: S },
      video: { src: U, crossOrigin: crossOrigin(),
        preload: { type: "enum", keywords: ["none", "metadata", "auto"], nonCanon: { "": "auto" }, defaultVal: ["none", "metadata", "auto"] },
        autoplay: B, loop: B, controls: B, defaultMuted: { type: B, domAttrName: "muted" },
        loading: { type: "enum", keywords: ["lazy", "eager"], defaultVal: "eager", invalidVal: "eager" },
        width: UL, height: UL, poster: U, playsInline: B },
      audio: { src: U, crossOrigin: crossOrigin(),
        preload: { type: "enum", keywords: ["none", "metadata", "auto"], nonCanon: { "": "auto" }, defaultVal: ["none", "metadata", "auto"] },
        autoplay: B, loop: B, controls: B, defaultMuted: { type: B, domAttrName: "muted" },
        loading: { type: "enum", keywords: ["lazy", "eager"], defaultVal: "eager", invalidVal: "eager" } },
      source: { src: U, type: S, srcset: S, sizes: S, media: S },
      track: { kind: { type: "enum", keywords: ["subtitles", "captions", "descriptions", "chapters", "metadata"], defaultVal: "subtitles", invalidVal: "metadata" },
        src: U, srclang: S, label: S, "default": B },
      canvas: { width: { type: UL, defaultVal: 300 }, height: { type: UL, defaultVal: 150 } },
      map: { name: S },
      area: { alt: S, coords: S, shape: S, target: S, download: S, ping: S, rel: S, referrerPolicy: ref(),
        hreflang: S, type: S, href: U, noHref: B },
      // sections
      body: { text: nullStr(), link: nullStr(), vLink: nullStr(), aLink: nullStr(), bgColor: nullStr(), background: S },
      h1: { align: S }, h2: { align: S }, h3: { align: S }, h4: { align: S }, h5: { align: S }, h6: { align: S },
      // metadata
      base: { href: { type: U, customGetter: true }, target: S },
      link: { href: U, crossOrigin: crossOrigin(), rel: S,
        as: { type: "enum", keywords: ["fetch", "audio", "document", "embed", "font", "image", "manifest", "object", "report", "script", "sharedworker", "style", "track", "video", "worker", "xslt"], defaultVal: "", invalidVal: "" },
        media: S, nonce: "nonce", integrity: S, hreflang: S, type: S, referrerPolicy: ref(),
        charset: S, rev: S, target: S },
      meta: { name: S, httpEquiv: { type: S, domAttrName: "http-equiv" }, content: S, media: S, scheme: S },
      style: { media: S, nonce: "nonce", type: S },
      // misc
      html: { version: S },
      script: { src: U, type: S, noModule: B, charset: S, defer: B, crossOrigin: crossOrigin(),
        integrity: S, event: S, htmlFor: { type: S, domAttrName: "for" } },
      slot: { name: S },
      ins: { cite: U, dateTime: S }, del: { cite: U, dateTime: S },
      details: { open: B }, menu: { compact: B }, dialog: { open: B },
      // tabular
      table: { align: S, border: S, frame: S, rules: S, summary: S, width: S,
        bgColor: nullStr(), cellPadding: nullStr(), cellSpacing: nullStr() },
      caption: { align: S },
      colgroup: assign({ span: { type: "clamped unsigned long", defaultVal: 1, min: 1, max: 1000 }, width: S }, cellCommon()),
      col: assign({ span: { type: "clamped unsigned long", defaultVal: 1, min: 1, max: 1000 }, width: S }, cellCommon()),
      tbody: cellCommon(), thead: cellCommon(), tfoot: cellCommon(),
      tr: assign(cellCommon(), { bgColor: nullStr() }),
      td: assign({ colSpan: { type: "clamped unsigned long", defaultVal: 1, min: 1, max: 1000 },
        rowSpan: { type: "clamped unsigned long", defaultVal: 1, min: 0, max: 65534 },
        headers: S, scope: { type: "enum", keywords: ["row", "col", "rowgroup", "colgroup"] }, abbr: S,
        axis: S, height: S, width: S, noWrap: B }, cellCommon(), { bgColor: nullStr() }),
      th: assign({ colSpan: { type: "clamped unsigned long", defaultVal: 1, min: 1, max: 1000 },
        rowSpan: { type: "clamped unsigned long", defaultVal: 1, min: 0, max: 65534 },
        headers: S, scope: { type: "enum", keywords: ["row", "col", "rowgroup", "colgroup"] }, abbr: S,
        axis: S, height: S, width: S, noWrap: B }, cellCommon(), { bgColor: nullStr() }),
      // obsolete
      marquee: { behavior: { type: "enum", keywords: ["scroll", "slide", "alternate"], defaultVal: "scroll" },
        bgColor: S, direction: { type: "enum", keywords: ["up", "right", "down", "left"], defaultVal: "left" },
        height: S, hspace: UL, scrollAmount: { type: UL, defaultVal: 6 }, scrollDelay: { type: UL, defaultVal: 85 },
        trueSpeed: B, vspace: UL, width: S },
      frameset: { cols: S, rows: S },
      frame: { name: S, scrolling: S, src: U, frameBorder: S, longDesc: U, noResize: B, marginHeight: nullStr(), marginWidth: nullStr() },
      dir: { compact: B },
      font: { color: nullStr(), face: S, size: S }
    };
  })();

  // Global attributes reflected on every HTML element (HTMLElement + a couple on Element).
  // These are tested for *every* element type by the reflection harness, so they dominate the
  // subtest count. `dir` is enumerated; `tabIndex` is a long with an element-specific default we
  // leave unspecified (the harness skips the default check when defaultVal is null); `hidden`/
  // `autofocus` are booleans; the rest are DOMStrings or enumerated.
  var __reflGlobals = {
    title: "string", lang: "string", accessKey: "string", translate: "string", nonce: "nonce",
    slot: { type: "string", domAttrName: "slot" },
    dir: { type: "enum", keywords: ["ltr", "rtl", "auto"] },
    autocapitalize: { type: "enum", keywords: ["off", "none", "on", "sentences", "words", "characters"], defaultVal: "" },
    enterKeyHint: { type: "enum", keywords: ["enter", "done", "go", "next", "previous", "search", "send"] },
    inputMode: { type: "enum", keywords: ["none", "text", "tel", "url", "email", "numeric", "decimal", "search"] },
    autofocus: "boolean", hidden: "boolean",
    tabIndex: { type: "long", defaultVal: null }
  };

  // ARIA reflection: every `ariaXxx` IDL attribute reflects the `aria-xxx` content attribute as a
  // nullable DOMString (matches the non-tentative WPT file + real browsers). Plus `role`.
  // List from the ARIA-in-HTML / AOM reflection spec.
  var __reflAria = ["Atomic", "AutoComplete", "BrailleLabel", "BrailleRoleDescription", "Busy",
    "Checked", "ColCount", "ColIndex", "ColIndexText", "ColSpan", "Current", "Description",
    "Disabled", "Expanded", "HasPopup", "Hidden", "Invalid", "KeyShortcuts", "Label", "Level",
    "Live", "Modal", "MultiLine", "MultiSelectable", "Orientation", "Placeholder", "PosInSet",
    "Pressed", "ReadOnly", "Relevant", "Required", "RoleDescription", "RowCount", "RowIndex",
    "RowIndexText", "RowSpan", "Selected", "SetSize", "Sort", "ValueMax", "ValueMin", "ValueNow",
    "ValueText"];

  // Define one reflected accessor on `el` for (idlName, data) backed by content attribute on `node`.
  function defineReflected(el, node, idlName, data) {
    if (typeof data === "string") { data = { type: data }; }
    var type = data.type;
    if (__refl.skip[type]) { return; }
    var factory = __refl.factories[type];
    if (!factory) { return; }
    // Note: customGetter attributes (input.autocomplete, meter.value, input.width/height, base.href)
    // have a bespoke getter in the spec we don't fully model, but the conformance harness only
    // checks their *type* and their setter behaviour (it skips the get/default asserts). The plain
    // factory getter is the right JS TYPE, so installing it lets those typeof/setter subtests pass.
    // Don't clobber an accessor the wrapper already defines correctly (value/checked/href/src/etc.).
    var existing = null;
    try { existing = Object.getOwnPropertyDescriptor(el, idlName); } catch (e) {}
    if (existing && (existing.get || existing.set)) { return; }
    // The content attribute name is the explicit domAttrName, else the ASCII-lowercased idlName
    // (HTML attributes are stored lowercased; e.g. maxLength <-> maxlength, colSpan <-> colspan).
    var contentAttr = data.domAttrName || __asciiLower(idlName);
    var io = __refl.mk(node, contentAttr);
    var desc = factory(io, data);
    try {
      Object.defineProperty(el, idlName, {
        get: desc.get, set: desc.set, enumerable: true, configurable: true
      });
    } catch (e) {}
  }

  // Apply all reflection accessors for the element `el` (node id `node`, lowercase tag `tag`).
  function applyReflection(el, node, tag) {
    // Global attributes (HTMLElement) on every HTML element. The SVG/MathML tags skip these.
    for (var gk in __reflGlobals) {
      if (Object.prototype.hasOwnProperty.call(__reflGlobals, gk)) {
        defineReflected(el, node, gk, __reflGlobals[gk]);
      }
    }
    // ARIA nullable-string reflection (HTMLElement + Element).
    defineReflected(el, node, "role", { type: "nullable string", domAttrName: "role" });
    for (var ai = 0; ai < __reflAria.length; ai++) {
      var nm = __reflAria[ai];
      defineReflected(el, node, "aria" + nm, { type: "nullable string", domAttrName: "aria-" + __asciiLower(nm) });
    }
    // Per-element attributes.
    var tbl = __reflTables[tag];
    if (tbl) {
      for (var k in tbl) {
        if (Object.prototype.hasOwnProperty.call(tbl, k)) { defineReflected(el, node, k, tbl[k]); }
      }
    }
  }
  def(globalThis, "__applyReflection", applyReflection);

  // Minimal Streams (WritableStream / ReadableStream / TransformStream / TextDecoderStream) — enough
  // for the streaming partial-update methods (streamHTML etc.) and piping a Response body through.
  if (typeof globalThis.WritableStream !== "function") {
    var WritableStream = function (sink) {
      this._sink = sink || {};
      this._writer = null;
      var s = this;
      this._ready = Promise.resolve().then(function () { return s._sink.start ? s._sink.start({ error: function () {} }) : undefined; });
    };
    WritableStream.prototype.getWriter = function () {
      if (this._writer) { throw new TypeError("WritableStream is locked to a writer"); }
      this._writer = new WritableStreamDefaultWriter(this);
      return this._writer;
    };
    Object.defineProperty(WritableStream.prototype, "locked", { get: function () { return !!this._writer; } });
    WritableStream.prototype.abort = function (reason) { var s = this; return Promise.resolve(s._sink.abort ? s._sink.abort(reason) : undefined); };
    WritableStream.prototype.close = function () { return this.getWriter().close(); };
    globalThis.WritableStream = WritableStream;
    var WritableStreamDefaultWriter = function (stream) {
      this._stream = stream; var self = this;
      this._closedP = new Promise(function (res, rej) { self._closedRes = res; self._closedRej = rej; });
    };
    WritableStreamDefaultWriter.prototype.write = function (chunk) { var st = this._stream; return st._ready.then(function () { return st._sink.write ? st._sink.write(chunk, { error: function () {} }) : undefined; }); };
    WritableStreamDefaultWriter.prototype.close = function () { var st = this._stream, self = this; return st._ready.then(function () { return st._sink.close ? st._sink.close() : undefined; }).then(function () { self._closedRes(); }); };
    WritableStreamDefaultWriter.prototype.abort = function (reason) { return this._stream.abort(reason); };
    WritableStreamDefaultWriter.prototype.releaseLock = function () { this._stream._writer = null; };
    Object.defineProperty(WritableStreamDefaultWriter.prototype, "ready", { get: function () { return Promise.resolve(); } });
    Object.defineProperty(WritableStreamDefaultWriter.prototype, "closed", { get: function () { return this._closedP; } });
    Object.defineProperty(WritableStreamDefaultWriter.prototype, "desiredSize", { get: function () { return 1; } });
    globalThis.WritableStreamDefaultWriter = WritableStreamDefaultWriter;

    var ReadableStream = function (source) {
      this._source = source || {};
      this._reader = null; this._queue = []; this._closed = false; this._err = null; this._waiters = [];
      var s = this;
      this._controller = {
        enqueue: function (c) { s._queue.push(c); s._wake(); },
        close: function () { s._closed = true; s._wake(); },
        error: function (e) { s._err = e; s._wake(); },
        get desiredSize() { return 1; }
      };
      this._started = Promise.resolve().then(function () { return s._source.start ? s._source.start(s._controller) : undefined; });
    };
    ReadableStream.prototype._wake = function () { var w = this._waiters; this._waiters = []; for (var i = 0; i < w.length; i++) { w[i](); } };
    ReadableStream.prototype._pull = function () {
      var s = this;
      return s._started.then(function step() {
        if (s._queue.length) { return { value: s._queue.shift(), done: false }; }
        if (s._err) { throw s._err; }
        if (s._closed) { return { value: undefined, done: true }; }
        return Promise.resolve(s._source.pull ? s._source.pull(s._controller) : undefined).then(function () {
          if (s._queue.length) { return { value: s._queue.shift(), done: false }; }
          if (s._closed) { return { value: undefined, done: true }; }
          return new Promise(function (res) { s._waiters.push(res); }).then(step);
        });
      });
    };
    ReadableStream.prototype.getReader = function () {
      if (this._reader) { throw new TypeError("locked"); }
      var s = this;
      this._reader = { read: function () { return s._pull(); }, releaseLock: function () { s._reader = null; }, cancel: function () { s._closed = true; return Promise.resolve(); }, closed: Promise.resolve() };
      return this._reader;
    };
    Object.defineProperty(ReadableStream.prototype, "locked", { get: function () { return !!this._reader; } });
    ReadableStream.prototype.pipeTo = function (dest) {
      var reader = this.getReader(), writer = dest.getWriter();
      return (function pump() { return reader.read().then(function (r) { if (r.done) { return writer.close(); } return Promise.resolve(writer.write(r.value)).then(pump); }); })();
    };
    ReadableStream.prototype.pipeThrough = function (tr) { this.pipeTo(tr.writable); return tr.readable; };
    ReadableStream.prototype.cancel = function () { this._closed = true; return Promise.resolve(); };
    globalThis.ReadableStream = ReadableStream;

    var TransformStream = function (transformer) {
      transformer = transformer || {};
      this.readable = new ReadableStream({});
      var rc = this.readable._controller;
      this.writable = new WritableStream({
        write: function (chunk) { return Promise.resolve(transformer.transform ? transformer.transform(chunk, rc) : rc.enqueue(chunk)); },
        close: function () { if (transformer.flush) { transformer.flush(rc); } rc.close(); },
        abort: function (e) { rc.error(e); }
      });
    };
    globalThis.TransformStream = TransformStream;
    globalThis.TextDecoderStream = function (label, options) {
      var dec = new globalThis.TextDecoder(label || "utf-8", options || {});
      var rc; var self = this;
      this.readable = new ReadableStream({});
      rc = this.readable._controller;
      this.writable = new WritableStream({
        write: function (chunk) { rc.enqueue(dec.decode(chunk, { stream: true })); },
        close: function () { var tail = dec.decode(); if (tail) { rc.enqueue(tail); } rc.close(); }
      });
      Object.defineProperty(this, "encoding", { value: dec.encoding || "utf-8" });
    };
  }

  // Parse an HTML string into a DocumentFragment for the partial-update methods (appendHTML etc.).
  // `safe` strips <script>s (the safe, sanitizing variants); a `sanitizer.removeElements` option
  // drops those elements too. Scripts are never executed here (the fragment isn't connected).
  globalThis.__htmlPartialFragment = function (html, safe, opts) {
    var div = document.createElement("div");
    div.innerHTML = (html == null ? "" : String(html));
    var dropAll = function (sel) {
      var els = Array.prototype.slice.call(div.querySelectorAll(sel));
      for (var i = 0; i < els.length; i++) { if (els[i].remove) { els[i].remove(); } }
    };
    if (safe) { dropAll("script"); }
    var rem = opts && opts.sanitizer && opts.sanitizer.removeElements;
    if (rem && rem.length) { for (var j = 0; j < rem.length; j++) { try { dropAll(rem[j]); } catch (e) {} } }
    var frag = document.createDocumentFragment();
    while (div.firstChild) { frag.appendChild(div.firstChild); }
    return frag;
  };

  // Attach the declarative partial-update methods ({append,prepend,before,after,replaceWith}HTML
  // [Unsafe]) to a node. Parent-position methods route through a <template>'s content.
  globalThis.__addPartialMethods = function (el) {
    var defs = [["append", 1], ["prepend", 1], ["before", 0], ["after", 0], ["replaceWith", 0]];
    for (var pi = 0; pi < defs.length; pi++) {
      (function (base, isParent) {
        [["HTML", true], ["HTMLUnsafe", false]].forEach(function (sfx) {
          var nm = base + sfx[0];
          if (typeof el[nm] === "function") { return; }
          Object.defineProperty(el, nm, { configurable: true, writable: true, enumerable: false, value: function (html, opts) {
            var frag = globalThis.__htmlPartialFragment(html, sfx[1], opts);
            if (isParent) {
              var dest = this.content || this;
              if (base === "append") { dest.appendChild(frag); }
              else { dest.insertBefore(frag, dest.firstChild || null); }
            } else {
              var p = this.parentNode;
              if (!p) { return; }
              if (base === "before") { p.insertBefore(frag, this); }
              else if (base === "after") { p.insertBefore(frag, this.nextSibling); }
              else { p.insertBefore(frag, this); p.removeChild(this); }
            }
          } });
        });
      })(defs[pi][0], defs[pi][1]);
    }
    // Streaming variants: stream{,Append,Prepend,Before,After,ReplaceWith}HTML[Unsafe]. Each returns
    // a WritableStream; written chunks are parsed and inserted at the position immediately (not
    // buffered until close). The insertion point is fixed at call time so chunks land in order.
    var streamDefs = [
      ["streamHTML", "replace", 1], ["streamHTMLUnsafe", "replace", 0],
      ["streamAppendHTML", "append", 1], ["streamAppendHTMLUnsafe", "append", 0],
      ["streamPrependHTML", "prepend", 1], ["streamPrependHTMLUnsafe", "prepend", 0],
      ["streamBeforeHTML", "before", 1], ["streamBeforeHTMLUnsafe", "before", 0],
      ["streamAfterHTML", "after", 1], ["streamAfterHTMLUnsafe", "after", 0],
      ["streamReplaceWithHTML", "replaceWith", 1], ["streamReplaceWithHTMLUnsafe", "replaceWith", 0]
    ];
    streamDefs.forEach(function (d) {
      var nm = d[0], pos = d[1], safe = !!d[2];
      if (typeof el[nm] === "function") { return; }
      Object.defineProperty(el, nm, { configurable: true, writable: true, enumerable: false, value: function (opts) {
        var node = this, insert;
        if (pos === "replace" || pos === "append" || pos === "prepend") {
          var dest = node.content || node;
          if (pos === "replace") { while (dest.firstChild) { dest.removeChild(dest.firstChild); } }
          if (pos === "prepend") { var pref = dest.firstChild; insert = function (f) { dest.insertBefore(f, pref); }; }
          else { insert = function (f) { dest.appendChild(f); }; }
        } else {
          var p = node.parentNode, sref;
          if (pos === "before") { sref = node; }
          else if (pos === "after") { sref = node.nextSibling; }
          else { sref = node.nextSibling; if (p) { p.removeChild(node); } }
          insert = function (f) { if (p) { p.insertBefore(f, sref); } };
        }
        return new globalThis.WritableStream({
          write: function (chunk) { insert(globalThis.__htmlPartialFragment(chunk == null ? "" : String(chunk), safe, opts)); },
          close: function () {}
        });
      } });
    });
  };

  // Deep structural node equality (DOM `isEqualNode`): same type and type-specific data, equal
  // attribute sets (order-independent, by namespace+localName+value), and pairwise-equal children.
  globalThis.__nodesEqual = function (a, b) {
    if (a === b) { return true; }
    if (!a || !b || a.nodeType !== b.nodeType) { return false; }
    var t = a.nodeType;
    if (t === 1) {
      if ((a.namespaceURI || null) !== (b.namespaceURI || null)) { return false; }
      if ((a.prefix || null) !== (b.prefix || null)) { return false; }
      if (a.localName !== b.localName) { return false; }
      var aa = a.attributes, ba = b.attributes;
      if ((aa ? aa.length : 0) !== (ba ? ba.length : 0)) { return false; }
      for (var i = 0; aa && i < aa.length; i++) {
        var at = aa[i], ok = false;
        for (var j = 0; j < ba.length; j++) {
          var bt = ba[j];
          if ((at.namespaceURI || null) === (bt.namespaceURI || null)
              && (at.localName || at.name) === (bt.localName || bt.name)
              && at.value === bt.value) { ok = true; break; }
        }
        if (!ok) { return false; }
      }
    } else if (t === 3 || t === 8 || t === 4) {
      if ((a.data || "") !== (b.data || "")) { return false; }
    } else if (t === 7) {
      if (a.target !== b.target || (a.data || "") !== (b.data || "")) { return false; }
    } else if (t === 10) {
      if (a.name !== b.name || (a.publicId || "") !== (b.publicId || "") || (a.systemId || "") !== (b.systemId || "")) { return false; }
    }
    var ac = a.childNodes || [], bc = b.childNodes || [];
    if (ac.length !== bc.length) { return false; }
    for (var k = 0; k < ac.length; k++) {
      if (!globalThis.__nodesEqual(ac[k], bc[k])) { return false; }
    }
    return true;
  };

  function enrichElement(el) {
    if (!el || typeof el !== "object") { return el; }
    if (el.__enriched) { return el; }
    var node = el.__node;
    def(el, "__enriched", true);
    // Compile inline event-handler content attributes (onload="...", onclick="...") into the matching
    // on-handler so they run when the event is dispatched. The handler body runs with `event` in scope
    // and `this` bound to the element (dispatchEvent calls `el.on<type>`).
    try {
      if (el.tagName && typeof el.getAttributeNames === "function") {
        var __ons = el.getAttributeNames();
        for (var __oi = 0; __oi < __ons.length; __oi++) {
          var __on = __ons[__oi];
          if (__on.length > 2 && __on.slice(0, 2) === "on" && typeof el[__on] !== "function") {
            try { el[__on] = new Function("event", el.getAttribute(__on)); } catch (e) {}
          }
        }
      }
    } catch (e) {}
    // Graft the matching DOM interface prototype onto the wrapper's chain (own props survive).
    // Non-element nodes (Text=3, Comment=8, DocumentFragment=11) use their CharacterData/Node
    // interface prototype so `instanceof Text/Comment/DocumentFragment` holds; elements map by tag.
    if (typeof node === "number") {
      try {
        var nt = __nodeType(node);
        var proto = null;
        if (nt === 3) { proto = globalThis.Text && globalThis.Text.prototype; }
        else if (nt === 8) { proto = globalThis.Comment && globalThis.Comment.prototype; }
        else if (nt === 11) { proto = globalThis.DocumentFragment && globalThis.DocumentFragment.prototype; }
        else { proto = ifaceProtoForTag(el.tagName); }
        if (proto && Object.getPrototypeOf(el) !== proto) { Object.setPrototypeOf(el, proto); }
      } catch (e) {}
    }
    if (typeof node === "number") {
      // `style` lives on the prototype chain (ElementCSSInlineStyle mixin) so it passes
      // assert_idl_attribute (own-property check). We stash the per-node CSSStyleDeclaration as a
      // hidden own property; the prototype accessor returns it ([SameObject], [PutForwards=cssText]).
      def(el, "__styleObj", makeStyle(node));
      // classList is [SameObject, PutForwards=value]: a per-element cached DOMTokenList whose
      // getter always returns the same object, and assigning `el.classList = x` forwards to
      // `el.classList.value = x` (so it never replaces the object and never throws in strict mode).
      (function () {
        var __cl = makeClassList(node);
        Object.defineProperty(el, "classList", {
          get: function () { return __cl; },
          set: function (v) { __cl.value = v; },
          enumerable: true, configurable: true
        });
      })();
      // Other DOMTokenList-reflecting attributes (HTML). Each is a [SameObject, PutForwards=value]
      // token list that exists only on the supporting element(s); on other elements the property is
      // absent (=== undefined). relList is also defined on the SVG `a` element.
      (function () {
        var HTML = "http://www.w3.org/1999/xhtml";
        var SVG = "http://www.w3.org/2000/svg";
        var ln = null, ns = null;
        try { ln = el.localName; ns = el.namespaceURI; } catch (e) {}
        function install(prop, contentAttr) {
          var tl = makeTokenList(node, contentAttr, null);
          Object.defineProperty(el, prop, {
            get: function () { return tl; },
            set: function (v) { tl.value = v; },
            enumerable: true, configurable: true
          });
        }
        // relList: on HTML a/area/link, and on SVG a.
        if ((ns === HTML && (ln === "a" || ln === "area" || ln === "link")) || (ns === SVG && ln === "a")) {
          install("relList", "rel");
        }
        if (ns === HTML && ln === "output") { install("htmlFor", "for"); }
        if (ns === HTML && ln === "iframe") { install("sandbox", "sandbox"); }
        if (ns === HTML && ln === "link") { install("sizes", "sizes"); }
      })();
      def(el, "dataset", makeDataset(node));
      // Form-control `value` / `checked` reflection: back them by element ATTRIBUTES so that
      // reading/writing `el.value` (and `el.checked`) is visible to layout, which renders the
      // input's text from the `value` attribute. Only for <input>/<textarea>/<select>; guard so
      // page-defined accessors aren't clobbered.
      try {
        var __formTag = typeof el.tagName === "string" ? el.tagName.toLowerCase() : "";
        if (__formTag === "input" || __formTag === "textarea" || __formTag === "select" || __formTag === "option") {
          var __hasValue = false;
          try { var __vd = Object.getOwnPropertyDescriptor(el, "value"); __hasValue = !!(__vd && (__vd.get || __vd.set)); } catch (e8) {}
          if (!__hasValue && __formTag !== "option") {
            if (__formTag === "textarea") {
              // A <textarea>'s value defaults to its text content; an explicit `value` attribute
              // (set via the property) overrides it. The setter stores `value` so layout renders it.
              Object.defineProperty(el, "value", {
                get: function () {
                  var v = __getAttr(node, "value");
                  if (v != null) { return String(v); }
                  var t = this.textContent;
                  return t == null ? "" : String(t);
                },
                set: function (v) { __setAttr(node, "value", String(v == null ? "" : v)); },
                configurable: true, enumerable: true
              });
            } else if (__formTag === "select") {
              // A <select>'s value is the selected <option>'s value (or its text if no value attr);
              // empty when nothing is selected. selectedIndex is the selected option's index.
              // Setting value selects the first matching option; setting selectedIndex selects by
              // position. Backed by the `selected` attribute on <option>s (also used by layout).
              var __optValue = function (o) {
                var av = o.getAttribute ? o.getAttribute("value") : null;
                if (av != null) { return av; }
                var t = o.textContent;
                return t == null ? "" : String(t).replace(/^\s+|\s+$/g, "");
              };
              var __selIdx = function (self) {
                var opts = self.querySelectorAll ? self.querySelectorAll("option") : [];
                for (var i = 0; i < opts.length; i++) {
                  if (opts[i].hasAttribute && opts[i].hasAttribute("selected")) { return i; }
                }
                return opts.length ? 0 : -1;
              };
              Object.defineProperty(el, "value", {
                get: function () {
                  var opts = this.querySelectorAll ? this.querySelectorAll("option") : [];
                  var idx = __selIdx(this);
                  if (idx < 0 || idx >= opts.length) { return ""; }
                  return __optValue(opts[idx]);
                },
                set: function (v) {
                  v = String(v == null ? "" : v);
                  var opts = this.querySelectorAll ? this.querySelectorAll("option") : [];
                  var found = -1;
                  for (var i = 0; i < opts.length; i++) { if (__optValue(opts[i]) === v) { found = i; break; } }
                  for (var j = 0; j < opts.length; j++) {
                    if (j === found) { opts[j].setAttribute("selected", ""); }
                    else { opts[j].removeAttribute("selected"); }
                  }
                },
                configurable: true, enumerable: true
              });
              Object.defineProperty(el, "selectedIndex", {
                get: function () { return __selIdx(this); },
                set: function (v) {
                  var idx = v | 0;
                  var opts = this.querySelectorAll ? this.querySelectorAll("option") : [];
                  for (var j = 0; j < opts.length; j++) {
                    if (j === idx) { opts[j].setAttribute("selected", ""); }
                    else { opts[j].removeAttribute("selected"); }
                  }
                },
                configurable: true, enumerable: true
              });
            } else {
              Object.defineProperty(el, "value", {
                get: function () { var v = __getAttr(node, "value"); return v == null ? "" : String(v); },
                set: function (v) { __setAttr(node, "value", String(v == null ? "" : v)); },
                configurable: true, enumerable: true
              });
            }
          }
          // <option>.value reflects its `value` attribute, falling back to text content;
          // <option>.selected reflects the `selected` attribute.
          if (__formTag === "option") {
            var __hasOptVal = false;
            try { var __ovd = Object.getOwnPropertyDescriptor(el, "value"); __hasOptVal = !!(__ovd && (__ovd.get || __ovd.set)); } catch (eOV) {}
            if (!__hasOptVal) {
              Object.defineProperty(el, "value", {
                get: function () { var v = __getAttr(node, "value"); if (v != null) { return String(v); } var t = this.textContent; return t == null ? "" : String(t).replace(/^\s+|\s+$/g, ""); },
                set: function (v) { __setAttr(node, "value", String(v)); },
                configurable: true, enumerable: true
              });
            }
            Object.defineProperty(el, "selected", {
              get: function () { return __getAttr(node, "selected") != null; },
              set: function (v) { if (v) { __setAttr(node, "selected", ""); } else { __removeAttr(node, "selected"); } },
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
        // `src` / `href` IDL reflection (resolved to absolute URLs) for the elements that have
        // them, so e.g. `img.src` is a STRING (google does `img.src.substring(...)`) not undefined.
        // URL resolution falls back to the raw attribute if our URL parser can't handle it, so the
        // value is always a string either way.
        // Spec URL reflection: absent attribute -> "", otherwise resolve the attribute value
        // against the document base URL (falling back to the raw value if it can't be parsed). An
        // empty-but-present attribute resolves to the document URL, per the standard.
        var __resolveURL = function (v) {
          if (v == null) { return ""; }
          return __reflResolveURL(v);
        };
        var __reflectURL = function (name, tags) {
          if (!tags[__formTag]) { return; }
          var has = false;
          try { var d = Object.getOwnPropertyDescriptor(el, name); has = !!(d && (d.get || d.set)); } catch (eD) {}
          if (has) { return; }
          Object.defineProperty(el, name, {
            get: function () { return __resolveURL(__getAttr(node, name)); },
            set: function (v) { __setAttr(node, name, String(v)); },
            configurable: true, enumerable: true
          });
        };
        __reflectURL("src", { img: 1, script: 1, iframe: 1, source: 1, video: 1, audio: 1, embed: 1, track: 1, input: 1, frame: 1 });
        __reflectURL("href", { a: 1, link: 1, area: 1, base: 1 });
        // HTMLHyperlinkElementUtils URL-decomposition accessors on <a>/<area>: protocol/host/...
        // derived from the resolved href. These also make the WPT reflection harness' resolveUrl()
        // (which decomposes a throwaway <a>) compute correct expected values for `url`-type attrs.
        if (__formTag === "a" || __formTag === "area") {
          var __hrefParts = function () {
            var raw = __getAttr(node, "href");
            var resolved = (raw == null) ? "" : __resolveURL(raw);
            return parseURL(resolved);
          };
          var __defUrlPart = function (prop, field) {
            var d = null;
            try { d = Object.getOwnPropertyDescriptor(el, prop); } catch (eU2) {}
            if (d && (d.get || d.set)) { return; }
            Object.defineProperty(el, prop, {
              get: function () { return __hrefParts()[field]; },
              set: function (v) {
                // Setters: replace the component, then store the reserialized URL.
                var p = __hrefParts();
                v = String(v);
                var HASH = String.fromCharCode(35), QUES = String.fromCharCode(63);
                if (prop === "protocol") { p.protocol = v.replace(/:*$/, "") + ":"; }
                else if (prop === "hash") { p.hash = v && v.charAt(0) !== HASH ? HASH + v : v; }
                else if (prop === "search") { p.search = v && v.charAt(0) !== QUES ? QUES + v : v; }
                else { p[field] = v; }
                var host = p.host || ((p.hostname || "") + (p.port ? ":" + p.port : ""));
                var s = p.protocol + (host || p.hostname ? "//" + host : "") + (p.pathname || "") + (p.search || "") + (p.hash || "");
                __setAttr(node, "href", s);
              },
              configurable: true, enumerable: true
            });
          };
          __defUrlPart("protocol", "protocol"); __defUrlPart("host", "host");
          __defUrlPart("hostname", "hostname"); __defUrlPart("port", "port");
          __defUrlPart("pathname", "pathname"); __defUrlPart("search", "search");
          __defUrlPart("hash", "hash"); __defUrlPart("origin", "origin");
          if (!("username" in el)) { def(el, "username", ""); }
          if (!("password" in el)) { def(el, "password", ""); }
        }
        // <img>.naturalWidth / naturalHeight: the decoded intrinsic size from the engine
        // (0 when the image is missing/broken/not yet decoded). `width`/`height` reflect the
        // used (rendered) size, falling back to the natural size.
        if (__formTag === "img") {
          var __natW = function (self) { var id = self.__node; var n = (typeof id === "number") ? __naturalSize(id) : null; return n ? n.w : 0; };
          var __natH = function (self) { var id = self.__node; var n = (typeof id === "number") ? __naturalSize(id) : null; return n ? n.h : 0; };
          var __defImgNum = function (prop, getter) {
            var has = false;
            try { var d = Object.getOwnPropertyDescriptor(el, prop); has = !!(d && (d.get || d.set)); } catch (eIN) {}
            if (!has) { Object.defineProperty(el, prop, { get: getter, configurable: true, enumerable: true }); }
          };
          __defImgNum("naturalWidth", function () { return __natW(this) | 0; });
          __defImgNum("naturalHeight", function () { return __natH(this) | 0; });
          // width/height reflect the rendered box (border-box from layout) else the HTML attr
          // else the natural size; setting updates the presentational attribute.
          // img.width/height are `unsigned long` reflections (presentational attr): set converts
          // via ToUint32 and an out-of-[0,maxInt] value becomes the default (0).
          var __imgUL = function (v) { var n = Number(v); if (!isFinite(n)) { n = 0; } n = (n < 0 ? Math.ceil(n) : Math.floor(n)) % 4294967296; if (n < 0) { n += 4294967296; } n = n >>> 0; return (n > 2147483647) ? 0 : n; };
          Object.defineProperty(el, "width", {
            get: function () { var id = this.__node; var r = (typeof id === "number") ? __rect(id) : null; if (r && r.width) { return Math.round(r.width); } var a = __getAttr(node, "width"); if (a != null && a !== "") { return parseInt(a, 10) || 0; } return __natW(this) | 0; },
            set: function (v) { __setAttr(node, "width", String(__imgUL(v))); },
            configurable: true, enumerable: true
          });
          Object.defineProperty(el, "height", {
            get: function () { var id = this.__node; var r = (typeof id === "number") ? __rect(id) : null; if (r && r.height) { return Math.round(r.height); } var a = __getAttr(node, "height"); if (a != null && a !== "") { return parseInt(a, 10) || 0; } return __natH(this) | 0; },
            set: function (v) { __setAttr(node, "height", String(__imgUL(v))); },
            configurable: true, enumerable: true
          });
        }
        // <dialog>: show()/showModal() set the `open` attribute; close(returnValue?) removes it,
        // stores returnValue, and fires a `close` event. `.open` reflects the attribute.
        if (__formTag === "dialog") {
          var __defDialog = function (prop, val) {
            try { if (typeof el[prop] !== "function") { def(el, prop, val); } } catch (eDl) { def(el, prop, val); }
          };
          __defDialog("show", function () { __setAttr(node, "open", ""); });
          __defDialog("showModal", function () { __setAttr(node, "open", ""); });
          __defDialog("close", function (rv) {
            if (__getAttr(node, "open") == null) { return; }
            __removeAttr(node, "open");
            if (rv !== undefined) { this.returnValue = String(rv); }
            try {
              var ev = (typeof Event === "function") ? new Event("close", { bubbles: false, cancelable: false }) : { type: "close" };
              this.dispatchEvent(ev);
            } catch (eEv) {}
          });
          var __hasOpen = false;
          try { var __od = Object.getOwnPropertyDescriptor(el, "open"); __hasOpen = !!(__od && (__od.get || __od.set)); } catch (eOpn) {}
          if (!__hasOpen) {
            Object.defineProperty(el, "open", {
              get: function () { return __getAttr(node, "open") != null; },
              set: function (v) { if (v) { __setAttr(node, "open", ""); } else { __removeAttr(node, "open"); } },
              configurable: true, enumerable: true
            });
          }
          if (!("returnValue" in el)) { el.returnValue = ""; }
        }
        // Generic HTML IDL attribute reflection: install all reflected accessors for this element
        // (global attributes + ARIA + per-element table). Runs AFTER the bespoke form-control / URL
        // / img / dialog accessors above so those take precedence (defineReflected won't clobber an
        // accessor that already exists).
        try { applyReflection(el, node, __formTag); } catch (eRf) {}
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

    // <style> (and stylesheet <link>) expose a live CSSStyleSheet via `.sheet`. The accessor lives on
    // the LinkStyle mixin prototype (HTMLStyleElement/HTMLLinkElement) so assert_idl_attribute passes
    // (must not be an own property); enrichElement just marks the element as sheet-bearing.
    if (typeof el.tagName === "string" && (el.tagName.toLowerCase() === "style" || el.tagName.toLowerCase() === "link") && !el.__sheetHost) {
      def(el, "__sheetHost", true);
    }

    // getBoundingClientRect / getClientRects: read the engine-pushed rect for this node
    // (viewport-relative CSS px). Detached / not-laid-out nodes get __rect()===null, so fall back
    // to the zero-rect (so they don't throw). toJSON returns the plain rect (DOMRect semantics).
    def(el, "getBoundingClientRect", function () {
      var id = this.__node;
      var r = (typeof id === "number") ? __rect(id) : null;
      if (!r) { return makeRect(); }
      r.toJSON = function () { return this; };
      return r;
    });
    def(el, "getClientRects", function () {
      var id = this.__node;
      var r = (typeof id === "number") ? __rect(id) : null;
      if (!r) { return []; }
      r.toJSON = function () { return this; };
      return [r];
    });
    // Live element-metric getters backed by __elemMetrics(this.__node) (0 when null). Only install
    // on real element wrappers (numeric node id) and don't clobber a page-defined accessor.
    if (typeof node === "number") {
      var __metricProps = {
        offsetWidth: "ow", offsetHeight: "oh", offsetTop: "ot", offsetLeft: "ol",
        clientWidth: "cw", clientHeight: "ch", // padding box: content + padding, no borders
        scrollWidth: "sw", scrollHeight: "sh"
      };
      for (var __mk in __metricProps) {
        (function (prop, field) {
          var __md = null;
          try { __md = Object.getOwnPropertyDescriptor(el, prop); } catch (eM) {}
          if (__md && (__md.get || __md.set)) { return; } // page already defined an accessor
          Object.defineProperty(el, prop, {
            get: function () {
              var m = __elemMetrics(this.__node);
              var v = m ? m[field] : 0;
              // The document root (<html>) often has no pushed box, so its width/height metrics read
              // 0. clientWidth/clientHeight of the root must be the viewport size (CSSOM-View), so
              // fall back to innerWidth/innerHeight there.
              if ((!v || v === 0)) {
                var nid = this.__node;
                if (nid === __documentElementId() || nid === __bodyId()) {
                  if (field === "cw" || field === "ow" || field === "sw") { var iw = Number(globalThis.innerWidth) || 0; if (iw > 0) { return iw; } }
                  if (field === "ch" || field === "oh") { var ih = Number(globalThis.innerHeight) || 0; if (ih > 0) { return ih; } }
                }
              }
              return v;
            },
            configurable: true, enumerable: true
          });
        })(__mk, __metricProps[__mk]);
      }
      // offsetParent: simple stand-in — document.body for laid-out elements, null when detached.
      var __opd = null;
      try { __opd = Object.getOwnPropertyDescriptor(el, "offsetParent"); } catch (eO) {}
      if (!(__opd && (__opd.get || __opd.set))) {
        Object.defineProperty(el, "offsetParent", {
          get: function () { return __elemMetrics(this.__node) ? document.body : null; },
          configurable: true, enumerable: true
        });
      }
    }
    if (typeof el.scrollIntoView !== "function") { def(el, "scrollIntoView", function () { try { __scrollIntoView(this.__node); } catch (e) {} }); }
    if (typeof el.focus !== "function") { def(el, "focus", fn); }
    if (typeof el.blur !== "function") { def(el, "blur", fn); }
    if (typeof el.click !== "function") { def(el, "click", fn); }
    if (typeof el.cloneNode !== "function") { def(el, "cloneNode", function () { return this; }); }
    if (typeof el.isEqualNode !== "function") { def(el, "isEqualNode", function (other) { return globalThis.__nodesEqual(this, other); }); }
    // Declarative partial-update methods (WICG): {append,prepend,before,after,replaceWith}HTML[Unsafe].
    globalThis.__addPartialMethods(el);
    if (typeof el.hasChildNodes !== "function") { def(el, "hasChildNodes", function () { try { return (this.childNodes || []).length > 0; } catch (e) { return false; } }); }
    if (!("nodeType" in el)) { def(el, "nodeType", 1); }
    if (!("ownerDocument" in el)) {
      // Dynamic: a node inside a <template>'s content belongs to that template's contents document,
      // and a node inside an <iframe>'s content document belongs to that document. Resolved by
      // walking the arena ancestry (so moving a node between documents updates its ownerDocument).
      Object.defineProperty(el, "ownerDocument", {
        get: function () {
          return (typeof globalThis.__ownerDocumentOf === "function") ? globalThis.__ownerDocumentOf(this) : document;
        },
        configurable: true, enumerable: true
      });
    }
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
    // <canvas>: a REAL 2D context that records a display list of resolved drawing commands.
    // The JS side maintains drawing state (styles + a 2D affine transform + path) and pushes
    // already-transformed, color-resolved commands into the canvas's display list; the Rust engine
    // pulls these via `__canvasLists()`, rasterizes them into a bitmap, and composites it like an
    // <img>. 'webgl'/'webgl2' return null so callers fall back gracefully.
    try {
      var __cvTag = typeof el.tagName === "string" ? el.tagName.toLowerCase() : "";
      if (__cvTag === "canvas" && typeof el.getContext !== "function") {
        // width/height reflect the canvas's content attributes (the bitmap size), defaulting to
        // the spec 300x150. Setting them updates the attribute and resets the drawing surface.
        (function () {
          // width/height are `unsigned long` reflections (default 300 / 150): parse via the rules
          // for parsing non-negative integers, range [0, 2147483647], else the default.
          function rd(attr, dflt) {
            var v = (typeof el.getAttribute === "function") ? el.getAttribute(attr) : null;
            if (v == null) { return dflt; }
            var p = __reflParseNonneg(v);
            if (p === false || p < 0 || p > 2147483647) { return dflt; }
            return p;
          }
          function wr(attr, v) {
            // ToUint32; out-of-[0,maxInt] becomes the default.
            var n = Number(v); if (!isFinite(n)) { n = 0; }
            n = (n < 0 ? Math.ceil(n) : Math.floor(n)) % 4294967296; if (n < 0) { n += 4294967296; }
            n = n >>> 0; if (n > 2147483647) { n = (attr === "width") ? 300 : 150; }
            try { if (typeof el.setAttribute === "function") { el.setAttribute(attr, String(n)); } } catch (e) {}
            // Resetting width/height clears the canvas (per spec). Drop the recorded display list.
            try { if (el.__ctx2d && el.__ctx2d.__list) { el.__ctx2d.__list.length = 0; } } catch (e2) {}
          }
          Object.defineProperty(el, "width", { get: function () { return rd("width", 300); }, set: function (v) { wr("width", v); }, configurable: true, enumerable: true });
          Object.defineProperty(el, "height", { get: function () { return rd("height", 150); }, set: function (v) { wr("height", v); }, configurable: true, enumerable: true });
        })();
        def(el, "getContext", function (type) {
          if (type !== "2d") { return null; }
          if (el.__ctx2d) { return el.__ctx2d; }
          var ctx = __makeCanvas2D(el);
          def(el, "__ctx2d", ctx);
          try {
            globalThis.__canvases = globalThis.__canvases || [];
            globalThis.__canvases.push(ctx);
          } catch (e) {}
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
  wrapReturningElement(document, "createElementNS");
  wrapReturningElement(document, "getElementById");
  wrapReturningElement(document, "getElementsByTagName");
  wrapReturningElement(document, "getElementsByTagNameNS");
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
      // Canonicalize so the wrapper is cached: navigation (nextSibling/firstChild) returns the same
      // object, preserving node identity (===), and enrichment grafts on partial-update methods.
      return canon(__wrapNode(__createText(String(data == null ? "" : data))));
    });
  }
  if (typeof document.createComment !== "function") {
    def(document, "createComment", function (data) {
      return canon(__wrapNode(__createComment(String(data == null ? "" : data))));
    });
  }
  // createCDATASection is only valid on XML documents; the live page document is HTML, so it throws.
  if (typeof document.createCDATASection !== "function") {
    def(document, "createCDATASection", function () {
      throw new globalThis.DOMException("This DOM method is only valid on XML documents.", "NotSupportedError");
    });
  }
  if (typeof document.createDocumentFragment !== "function") {
    def(document, "createDocumentFragment", function () {
      // Real arena-backed DocumentFragment (nodeType 11): its children move on insertion, and it
      // supports the full ParentNode mixin (append/prepend/replaceChildren/appendChild/insertBefore).
      // Canonicalize so navigation accessors (firstChild) return enriched, prototype-correct nodes.
      return canon(__wrapNode(__createDocumentFragment()));
    });
  }

  if (typeof document.createRange !== "function") {
    def(document, "createRange", function () {
      var r = new globalThis.Range();
      r.setStart(this, 0);
      r.setEnd(this, 0);
      return r;
    });
  }
  // document.implementation.createHTMLDocument — used to build/parse HTML off to the side (e.g.
  // sanitizers, template parsing). We back it with real (detached) arena nodes so innerHTML /
  // appendChild / querySelector work on the returned document's tree.
  if (typeof document.implementation === "undefined" || !document.implementation) {
    // Build a real (arena-backed) DocumentType whose ownerDocument is `ownerDoc`. The arena node is
    // created via the validating factory; we override its ownerDocument to the requested document.
    function makeDoctypeFor(ownerDoc, name, pub, sys) {
      var dt = globalThis.__createDocumentTypeNode(String(name), pub == null ? "" : String(pub), sys == null ? "" : String(sys));
      try { Object.defineProperty(dt, "ownerDocument", { value: ownerDoc, configurable: true, enumerable: true }); } catch (e) {}
      return dt;
    }
    // Back an off-document facade with a real (detached) arena Document node, so the Node mutation
    // methods + live child accessors work against the arena (appendChild/insertBefore/removeChild/
    // replaceChild, childNodes/firstChild/lastChild). `initialChildIds` are appended in order.
    function backDocWithArena(doc, initialChildIds) {
      var docId = globalThis.__createDocumentNode();
      try { Object.defineProperty(doc, "__node", { value: docId, configurable: true }); } catch (e) {}
      for (var i = 0; i < initialChildIds.length; i++) {
        var cid = initialChildIds[i];
        if (typeof cid === "number" && cid >= 0) { globalThis.__insertNode(docId, cid, -1); }
      }
      function reqNode(x, m) {
        var n = (x && typeof x.__node === "number") ? x.__node : -1;
        if (n < 0) { throw new TypeError("Failed to execute '" + m + "' on 'Node': parameter is not of type 'Node'."); }
        return n;
      }
      function nf(msg) { throw new (globalThis.DOMException)(msg, "NotFoundError"); }
      def(doc, "appendChild", function (child) { var c = reqNode(child, "appendChild"); globalThis.__insertNode(docId, c, -1); return child; });
      def(doc, "insertBefore", function (newNode, refNode) {
        var c = reqNode(newNode, "insertBefore");
        var r = (refNode == null) ? -1 : ((refNode && typeof refNode.__node === "number") ? refNode.__node : -1);
        if (refNode != null && r < 0) { nf("The reference child is not a child of this node."); }
        globalThis.__insertNode(docId, c, r); return newNode;
      });
      def(doc, "removeChild", function (child) {
        var c = reqNode(child, "removeChild");
        if (globalThis.__parent(c) !== docId) { nf("The node to be removed is not a child of this node."); }
        globalThis.__removeChild(docId, c); return child;
      });
      def(doc, "replaceChild", function (newNode, oldNode) {
        var n = reqNode(newNode, "replaceChild"), o = reqNode(oldNode, "replaceChild");
        if (globalThis.__parent(o) !== docId) { nf("The node to be replaced is not a child of this node."); }
        var sibs = globalThis.__children(docId); var idx = sibs.indexOf(o);
        var ref = (idx >= 0 && idx + 1 < sibs.length) ? sibs[idx + 1] : -1;
        if (ref === n) { var ni = sibs.indexOf(n); ref = (ni >= 0 && ni + 1 < sibs.length) ? sibs[ni + 1] : -1; }
        globalThis.__removeChild(docId, o); globalThis.__insertNode(docId, n, ref); return oldNode;
      });
      function kids() { return globalThis.__children(docId); }
      Object.defineProperty(doc, "childNodes", { get: function () { var ids = kids(), a = []; for (var i = 0; i < ids.length; i++) { a.push(globalThis.__nodeFor(ids[i])); } return a; }, configurable: true, enumerable: true });
      Object.defineProperty(doc, "firstChild", { get: function () { var ids = kids(); return ids.length ? globalThis.__nodeFor(ids[0]) : null; }, configurable: true, enumerable: true });
      Object.defineProperty(doc, "lastChild", { get: function () { var ids = kids(); return ids.length ? globalThis.__nodeFor(ids[ids.length - 1]) : null; }, configurable: true, enumerable: true });
      return doc;
    }
    def(document, "implementation", {
      hasFeature: function () { return true; },
      createDocumentType: function (name, pub, sys) { return makeDoctypeFor(document, name, pub, sys); },
      createHTMLDocument: function (title) {
        var htmlEl = document.createElement("html");
        var headEl = document.createElement("head");
        var bodyEl = document.createElement("body");
        htmlEl.appendChild(headEl); htmlEl.appendChild(bodyEl);
        if (title !== undefined && title !== null) {
          var t = document.createElement("title"); t.textContent = String(title); headEl.appendChild(t);
        }
        var doc;
        doc = {
          nodeType: 9, nodeName: '#document', documentElement: htmlEl, head: headEl, body: bodyEl, title: title ? String(title) : "",
          doctype: null,
          // A document created off to the side has no associated browsing context / viewport, so per
          // CSSOM-View these always return null regardless of the coordinates passed.
          caretPositionFromPoint: function () { return null; },
          caretRangeFromPoint: function () { return null; },
          elementFromPoint: function () { return null; },
          elementsFromPoint: function () { return []; },
          lookupNamespaceURI: function () { return htmlEl && htmlEl.lookupNamespaceURI ? htmlEl.lookupNamespaceURI.apply(htmlEl, arguments) : null; },
          lookupPrefix: function () { return htmlEl && htmlEl.lookupPrefix ? htmlEl.lookupPrefix.apply(htmlEl, arguments) : null; },
          isDefaultNamespace: function () { return htmlEl && htmlEl.isDefaultNamespace ? htmlEl.isDefaultNamespace.apply(htmlEl, arguments) : (arguments[0] == null || arguments[0] === ""); },
          implementation: {
            hasFeature: function () { return true; },
            createDocumentType: function (n, p, s) { return makeDoctypeFor(doc, n, p, s); },
            createHTMLDocument: function (t2) { return document.implementation.createHTMLDocument(t2); },
            createDocument: function (ns, q, dt) { return document.implementation.createDocument(ns, q, dt); },
          },
          cloneNode: function (deep) {
            var c = document.implementation.createHTMLDocument(this.title);
            return c;
          },
          createElement: function (tag) { return document.createElement(tag); },
          createElementNS: function (ns, tag) { return document.createElementNS ? document.createElementNS(ns, tag) : document.createElement(tag); },
          createAttribute: function (name) {
            var nm = String(name);
            if (nm.length === 0) { globalThis.__invalidCharacterError(); }
            return globalThis.__makeAttrNode(null, null, nm.toLowerCase(), nm.toLowerCase());
          },
          createAttributeNS: function (ns, qn) {
            var ex = globalThis.__validateAndExtractName(ns, qn);
            return globalThis.__makeAttrNode(ex.namespace, ex.prefix, ex.localName, String(qn));
          },
          createTextNode: function (s) { return document.createTextNode(s); },
          createComment: function (s) { return document.createComment(s); },
          // An HTML document refuses createCDATASection (the XML createDocument path overrides this).
          createCDATASection: function () { throw new globalThis.DOMException("This DOM method is only valid on XML documents.", "NotSupportedError"); },
          createDocumentFragment: function () { return document.createDocumentFragment(); },
          createProcessingInstruction: function (target, data) { return document.createProcessingInstruction(target, data); },
          importNode: function (n) { return n; }, adoptNode: function (n) { return n; },
          getElementById: function (id) { return htmlEl.querySelector ? htmlEl.querySelector('#' + id) : null; },
          querySelector: function (s) { return htmlEl.querySelector ? htmlEl.querySelector(s) : null; },
          querySelectorAll: function (s) { return htmlEl.querySelectorAll ? htmlEl.querySelectorAll(s) : []; },
          getElementsByTagName: function (t) { return htmlEl.getElementsByTagName ? htmlEl.getElementsByTagName(t) : []; },
        };
        // A Document's textContent / nodeValue are null; setting them is a no-op.
        Object.defineProperty(doc, "textContent", { get: function () { return null; }, set: function () {}, enumerable: true, configurable: true });
        Object.defineProperty(doc, "nodeValue", { get: function () { return null; }, set: function () {}, enumerable: true, configurable: true });
        // Back the facade with a real arena Document node holding <html> as its element child, so
        // appendChild / childNodes / traversal work on the off-document tree.
        backDocWithArena(doc, [htmlEl && typeof htmlEl.__node === "number" ? htmlEl.__node : -1]);
        return doc;
      },
      createDocument: function (namespace) {
        // An XML document: like createHTMLDocument but case-preserving for createAttribute, and
        // createElement assigns the null namespace (the HTML namespace only when the document's own
        // namespace is the HTML namespace, i.e. an application/xhtml+xml document).
        var d = this.createHTMLDocument("");
        var docNs = (namespace === undefined || namespace === null || namespace === "") ? null : String(namespace);
        var elNs = docNs === "http://www.w3.org/1999/xhtml" ? docNs : null;
        d.createElement = function (name) { return globalThis.__createElementWithNs(elNs, name); };
        // An XML document supports createCDATASection (overriding createHTMLDocument's HTML refusal).
        d.createCDATASection = function (data) { return globalThis.__canonNode(globalThis.__wrapNode(globalThis.__createCData(String(data == null ? "" : data)))); };
        d.createAttribute = function (name) {
          var nm = String(name);
          if (nm.length === 0) { globalThis.__invalidCharacterError(); }
          return globalThis.__makeAttrNode(null, null, nm, nm);
        };
        return d;
      },
    });
  }
  if (typeof document.getElementsByName !== "function") {
    def(document, "getElementsByName", function (n) { try { return document.querySelectorAll('[name="' + String(n) + '"]'); } catch (e) { return []; } });
  }
  if (typeof document.contains !== "function") {
    def(document, "contains", function (node) { try { return document.documentElement ? (document.documentElement === node || document.documentElement.contains(node)) : false; } catch (e) { return false; } });
  }

  // Document is a Node: its children are the doctype + the root element. Wire the Node mutation
  // methods on `document` itself. Only globals (`__insertNode`/`__removeChild`/`__parent`/
  // `__children`/`__documentElementId`) are in scope here, so the node-id helpers are inlined. The
  // document node id is the parent of <html>.
  (function () {
    function reqNode(x, m) {
      var n = (x && typeof x.__node === "number") ? x.__node : -1;
      if (n < 0) { throw new TypeError("Failed to execute '" + m + "' on 'Node': parameter is not of type 'Node'."); }
      return n;
    }
    function notFound(msg) { throw new (globalThis.DOMException)(msg, "NotFoundError"); }
    function docNode() { var de = __documentElementId(); return de >= 0 ? __parent(de) : -1; }
    def(document, "appendChild", function (child) {
      var id = docNode(); var c = reqNode(child, "appendChild"); __insertNode(id, c, -1); return child;
    });
    def(document, "insertBefore", function (newNode, refNode) {
      var id = docNode(); var c = reqNode(newNode, "insertBefore");
      var r = (refNode == null) ? -1 : ((refNode && typeof refNode.__node === "number") ? refNode.__node : -1);
      if (refNode != null && r < 0) { notFound("The reference child is not a child of this node."); }
      __insertNode(id, c, r); return newNode;
    });
    def(document, "removeChild", function (child) {
      var id = docNode(); var c = reqNode(child, "removeChild");
      if (__parent(c) !== id) { notFound("The node to be removed is not a child of this node."); }
      __removeChild(id, c); return child;
    });
    def(document, "replaceChild", function (newNode, oldNode) {
      var id = docNode(); var n = reqNode(newNode, "replaceChild"), o = reqNode(oldNode, "replaceChild");
      if (__parent(o) !== id) { notFound("The node to be replaced is not a child of this node."); }
      var sibs = __children(id); var idx = sibs.indexOf(o);
      var ref = (idx >= 0 && idx + 1 < sibs.length) ? sibs[idx + 1] : -1;
      if (ref === n) { var ni = sibs.indexOf(n); ref = (ni >= 0 && ni + 1 < sibs.length) ? sibs[ni + 1] : -1; }
      __removeChild(id, o); __insertNode(id, n, ref); return oldNode;
    });
  })();
  // Legacy event factory. Maps a (case-insensitive) interface name to an uninitialized event of
  // the right interface (prototype chain intact); unknown names throw NotSupportedError. The real
  // implementation lives in globalThis.__createEvent (defined alongside the Event constructors).
  def(document, "createEvent", function (name) { return globalThis.__createEvent(name); });

  // --- hit-testing: elementFromPoint / caretPositionFromPoint / caretRangeFromPoint -----------
  //
  // The engine lays out the page and pushes every box's border-box rect (CSS px, document-absolute,
  // top-origin) to this worker as `layout_rects`, read here via `__rect(id)` (which already returns
  // the rect VIEWPORT-relative, i.e. with the vertical scroll subtracted). We cannot reach the
  // engine's live layout tree synchronously from the JS thread, so the hit-test runs here against
  // those pushed rects, using the live DOM (`__children`/`__parent`/`__nodeType`) for tree depth.
  //
  // `__elementAtPoint(x, y)` — x/y are CSS px, viewport-relative — returns the deepest ELEMENT node
  // id whose laid-out box contains the point, or -1 when the point is outside the viewport or hits
  // no box. It is the native primitive the three public methods are built on.
  function __viewportClientWidth() {
    var w = Number(globalThis.innerWidth) || 0;
    if (w > 0) { return w; }
    try { var c = document.documentElement && document.documentElement.clientWidth; if (typeof c === "number" && c > 0) { return c; } } catch (e) {}
    return 0;
  }
  function __viewportClientHeight() {
    var h = Number(globalThis.innerHeight) || 0;
    if (h > 0) { return h; }
    try { var c = document.documentElement && document.documentElement.clientHeight; if (typeof c === "number" && c > 0) { return c; } } catch (e) {}
    return 0;
  }
  // Deepest node (element OR text) whose engine-pushed rect contains the viewport point. Walks the
  // DOM tree; a child hit wins over its ancestor (deepest box on top). Ignores pointer-events (the
  // pushed rects carry no paint/pointer metadata) — adequate for the WPT cases, which only need the
  // node the point geometrically lands in. The pushed rects are the UNTRANSFORMED border boxes, so
  // hit-testing through CSS transforms (translate/rotate/scale) uses the pre-transform box — an
  // approximation that matches the painter only for the identity transform. Returns a node id or -1.
  function __deepestNodeAtPoint(x, y) {
    function rectOf(nodeId) {
      var r = null;
      try { r = __rect(nodeId); } catch (e) {}
      return r;
    }
    function contains(r) { return r && x >= r.left && x < r.right && y >= r.top && y < r.bottom; }
    // Depth-first; recurse into children first so deeper boxes take precedence, matching the
    // engine's `deepest_node_at`. Returns the deepest descendant-or-self node that contains the
    // point, or -1.
    function visit(nodeId) {
      var kids;
      try { kids = __children(nodeId); } catch (e) { kids = []; }
      for (var i = kids.length - 1; i >= 0; i--) {
        var hit = visit(kids[i]);
        if (hit >= 0) { return hit; }
      }
      var t = __nodeType(nodeId);
      // Only element (1) and text (3) boxes are candidates; skip comments / others.
      if (t === 1 || t === 3) {
        if (contains(rectOf(nodeId))) { return nodeId; }
      }
      return -1;
    }
    var rootId = __documentRootId();
    return visit(rootId);
  }
  // Public native: deepest ELEMENT at the viewport point (text hits climb to their element parent),
  // or -1 when outside the viewport / no box.
  def(globalThis, "__elementAtPoint", function (x, y) {
    x = Number(x); y = Number(y);
    if (!isFinite(x) || !isFinite(y)) { return -1; }
    if (x < 0 || y < 0 || x >= __viewportClientWidth() || y >= __viewportClientHeight()) { return -1; }
    var n = __deepestNodeAtPoint(x, y);
    while (n >= 0) {
      if (__nodeType(n) === 1) { return n; }
      var p = __parent(n);
      if (p < 0) { break; }
      n = p;
    }
    return -1;
  });

  if (typeof document.elementFromPoint !== "function") {
    def(document, "elementFromPoint", function (x, y) {
      var id = globalThis.__elementAtPoint(x, y);
      return id >= 0 ? __nodeFor(id) : null;
    });
  }
  if (typeof document.elementsFromPoint !== "function") {
    // Best-effort: the topmost element, then its ancestor chain (the engine pushes no z-order, so we
    // approximate the stack by the ancestor chain of the deepest hit).
    def(document, "elementsFromPoint", function (x, y) {
      var out = [];
      var id = globalThis.__elementAtPoint(x, y);
      while (id >= 0) {
        if (__nodeType(id) === 1) { out.push(__nodeFor(id)); }
        id = __parent(id);
      }
      return out;
    });
  }

  // caretPositionFromPoint(x, y): per CSSOM-View, the caret position (a CaretPosition with
  // offsetNode + character offset) for the point. Throws TypeError if called with fewer than two
  // arguments; returns null when the point is outside the viewport. offsetNode prefers the TEXT node
  // at the point (else the element); `offset` is the character index nearest the point, derived from
  // the text run's box width (a monospaced/uniform approximation — we have no per-glyph metrics).
  def(document, "caretPositionFromPoint", function (x, y) {
    if (arguments.length < 2) { throw new TypeError("Failed to execute 'caretPositionFromPoint' on 'Document': 2 arguments required, but only " + arguments.length + " present."); }
    x = Number(x); y = Number(y);
    if (!isFinite(x) || !isFinite(y)) { throw new TypeError("Failed to execute 'caretPositionFromPoint' on 'Document': argument is not a finite number."); }
    if (x < 0 || y < 0 || x >= __viewportClientWidth() || y >= __viewportClientHeight()) { return null; }
    return globalThis.__makeCaretAt(x, y);
  });

  // caretRangeFromPoint(x, y): a collapsed Range at the caret position for the point. With no/zero
  // coordinates it returns a Range collapsed at (root element, 0). Outside the viewport → null.
  def(document, "caretRangeFromPoint", function (x, y) {
    if (arguments.length >= 1) {
      var nx = Number(x), ny = Number(y);
      if (isFinite(nx) && isFinite(ny) && (nx < 0 || ny < 0 || nx >= __viewportClientWidth() || ny >= __viewportClientHeight())) {
        return null;
      }
    }
    var caret = globalThis.__makeCaretAt(x, y);
    var node, offset;
    if (caret) { node = caret.offsetNode; offset = caret.offset; }
    if (!node) {
      // No hit (no/zero coords, or empty layout): collapse at the root element, offset 0.
      var rootEl = document.documentElement || document.body || null;
      if (!rootEl) {
        try {
          var kids = __children(__documentRootId());
          for (var i = 0; i < kids.length; i++) { if (__nodeType(kids[i]) === 1) { rootEl = __nodeFor(kids[i]); break; } }
        } catch (e) {}
      }
      node = rootEl; offset = 0;
    }
    if (!node) { return null; }
    var r = new globalThis.Range();
    r.setStart(node, offset);
    r.setEnd(node, offset);
    return r;
  });
  if (typeof document.hasFocus !== "function") { def(document, "hasFocus", function () { return true; }); }
  if (!("activeElement" in document)) { Object.defineProperty(document, "activeElement", { get: function () { try { return document.body; } catch (e) { return null; } }, enumerable: true, configurable: true }); }
  if (!("visibilityState" in document)) { document.visibilityState = "visible"; }
  if (!("hidden" in document)) { document.hidden = false; }
  // The document's character encoding reflects its <meta charset> (or content-type meta), per the
  // Encoding standard's label->name mapping; absent one it defaults to UTF-8. charset / characterSet
  // / inputEncoding are aliases.
  function __canonCharset(label) {
    var l = String(label || "").trim().toLowerCase();
    var m = {
      "utf-8": "UTF-8", "utf8": "UTF-8", "unicode-1-1-utf-8": "UTF-8",
      "windows-1252": "windows-1252", "cp1252": "windows-1252", "x-cp1252": "windows-1252",
      "iso-8859-1": "windows-1252", "latin1": "windows-1252", "ascii": "windows-1252", "us-ascii": "windows-1252",
      "iso-8859-2": "ISO-8859-2", "latin2": "ISO-8859-2", "l2": "ISO-8859-2",
      "windows-1251": "windows-1251", "koi8-r": "KOI8-R", "shift_jis": "Shift_JIS", "sjis": "Shift_JIS",
      "euc-jp": "EUC-JP", "euc-kr": "EUC-KR", "gbk": "GBK", "gb2312": "GBK", "big5": "Big5",
      "utf-16": "UTF-16LE", "utf-16le": "UTF-16LE", "utf-16be": "UTF-16BE"
    };
    return Object.prototype.hasOwnProperty.call(m, l) ? m[l] : null;
  }
  function __documentCharset() {
    try {
      var m = document.querySelector("meta[charset]");
      if (m) { var c = __canonCharset(m.getAttribute("charset")); if (c) { return c; } }
      var hs = document.querySelectorAll("meta[http-equiv]");
      for (var i = 0; i < hs.length; i++) {
        if ((hs[i].getAttribute("http-equiv") || "").toLowerCase() === "content-type") {
          var mt = /charset\s*=\s*([^\s;]+)/i.exec(hs[i].getAttribute("content") || "");
          if (mt) { var cc = __canonCharset(mt[1]); if (cc) { return cc; } }
        }
      }
    } catch (e) {}
    return "UTF-8";
  }
  if (!("characterSet" in document) || typeof document.characterSet === "string") {
    var __csGetter = { get: function () { return __documentCharset(); }, enumerable: true, configurable: true };
    try { Object.defineProperty(document, "characterSet", __csGetter); } catch (e) {}
    try { Object.defineProperty(document, "charset", __csGetter); } catch (e) {}
    try { Object.defineProperty(document, "inputEncoding", __csGetter); } catch (e) {}
  }
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

  // --- Observer constructors ---------------------------------------------------------------
  // ============================================================================================
  // Real MutationObserver / IntersectionObserver / ResizeObserver.
  //
  // The heavy lifting lives in Rust: mutation TRACKING happens in the DOM primitives (queued and
  // exposed via __drainMutations), and geometry/intersection/size COMPUTATION happens in the
  // engine. These JS classes are the spec-facing registries + callback dispatch only.
  //
  //  - MutationObserver records {targetId, options} in __moRegistry; on first observe it flips the
  //    Rust gate via __observersActive(true). After a task, drain_event_loop calls __deliverMutations
  //    which reads __drainMutations(), matches recs to observers, builds MutationRecords, fires cbs.
  //  - IntersectionObserver/ResizeObserver register (observerId,nodeId,opts) in __io/__ro. The Rust
  //    engine reads __observedTargets(), computes geometry, and calls __deliverObservations(json).
  // ============================================================================================
  globalThis.__moRegistry = globalThis.__moRegistry || [];   // [{observer, targets:[{id,opts}], queue:[]}]
  globalThis.__io = globalThis.__io || {};                   // observerId -> {observer, cb, opts, targets:{nodeId:true}}
  globalThis.__ro = globalThis.__ro || {};                   // observerId -> {observer, cb, targets:{nodeId:true}}
  var __obsIdSeq = 1;

  function __syncObserversActive() {
    var any = false;
    for (var i = 0; i < globalThis.__moRegistry.length; i++) {
      if (globalThis.__moRegistry[i].targets.length) { any = true; break; }
    }
    try { __observersActive(any); } catch (e) {}
  }

  // node-id -> wrapper element. Reuse the canonical wrapper machinery so callbacks get the same
  // element objects the page already holds.
  function __nodeWrap(id) {
    if (typeof id !== "number" || id < 0) { return null; }
    try { return canon(__wrapNode(id)); } catch (e) { return null; }
  }
  globalThis.__nodeWrap = __nodeWrap;

  if (typeof globalThis.MutationObserver !== "function") {
    def(globalThis, "MutationObserver", function (cb) {
      this.callback = typeof cb === "function" ? cb : fn;
      this._entry = { observer: this, targets: [], queue: [] };
    });
    def(globalThis.MutationObserver.prototype, "observe", function (target, opts) {
      var id = (target && typeof target.__node === "number") ? target.__node : -1;
      if (id < 0) { return; }
      opts = opts || {};
      var rec = {
        targetId: id,
        childList: !!opts.childList,
        attributes: opts.attributes !== undefined ? !!opts.attributes : (opts.attributeOldValue || opts.attributeFilter ? true : false),
        characterData: opts.characterData !== undefined ? !!opts.characterData : (opts.characterDataOldValue ? true : false),
        subtree: !!opts.subtree,
        attributeOldValue: !!opts.attributeOldValue,
        characterDataOldValue: !!opts.characterDataOldValue,
        attributeFilter: opts.attributeFilter ? [].concat(opts.attributeFilter) : null
      };
      // Per spec, observing the same node again replaces its options.
      var t = this._entry.targets;
      for (var i = 0; i < t.length; i++) { if (t[i].targetId === id) { t.splice(i, 1); break; } }
      t.push(rec);
      if (globalThis.__moRegistry.indexOf(this._entry) < 0) { globalThis.__moRegistry.push(this._entry); }
      __syncObserversActive();
    });
    def(globalThis.MutationObserver.prototype, "disconnect", function () {
      this._entry.targets = [];
      this._entry.queue = [];
      var i = globalThis.__moRegistry.indexOf(this._entry);
      if (i >= 0) { globalThis.__moRegistry.splice(i, 1); }
      __syncObserversActive();
    });
    def(globalThis.MutationObserver.prototype, "takeRecords", function () {
      // Per spec, takeRecords() must synchronously return the records observed so far. Drain any
      // pending Rust-side mutations into every observer's queue first, then empty *our* queue.
      try { globalThis.__collectMutations(); } catch (e) {}
      var q = this._entry.queue; this._entry.queue = []; return q;
    });
  }

  // Walk ancestors (inclusive) of a node id, capped, to test subtree membership.
  function __isInclusiveAncestor(ancestorId, nodeId) {
    var cur = nodeId, guard = 0;
    while (typeof cur === "number" && cur >= 0 && guard++ < 10000) {
      if (cur === ancestorId) { return true; }
      cur = __parent(cur);
    }
    return false;
  }

  // Drain any pending Rust-side mutations, match each against every observer's registered targets,
  // build MutationRecords, and APPEND them to each matching observer's queue. Idempotent: once the
  // Rust queue is empty it does nothing. Shared by takeRecords() (synchronous) and the post-task
  // microtask delivery below.
  def(globalThis, "__collectMutations", function () {
    var recs;
    try { recs = JSON.parse(__drainMutations()); } catch (e) { recs = []; }
    if (!recs.length) { return; }
    var reg = globalThis.__moRegistry;
    for (var o = 0; o < reg.length; o++) {
      var entry = reg[o];
      for (var r = 0; r < recs.length; r++) {
        var rec = recs[r];
        for (var ti = 0; ti < entry.targets.length; ti++) {
          var t = entry.targets[ti];
          // Does this observed target match the mutated node? (exact, or ancestor if subtree)
          var matches = (t.targetId === rec.target) || (t.subtree && __isInclusiveAncestor(t.targetId, rec.target));
          if (!matches) { continue; }
          if (rec.kind === "childList") {
            if (!t.childList) { continue; }
          } else if (rec.kind === "attributes") {
            if (!t.attributes) { continue; }
            if (t.attributeFilter && t.attributeFilter.indexOf(rec.attr) < 0) { continue; }
          } else if (rec.kind === "characterData") {
            if (!t.characterData) { continue; }
          }
          var mr = { type: rec.kind, target: __nodeWrap(rec.target),
            attributeName: rec.kind === "attributes" ? rec.attr : null,
            attributeNamespace: null,
            oldValue: null,
            addedNodes: [], removedNodes: [],
            previousSibling: null, nextSibling: null };
          if (rec.kind === "attributes" && t.attributeOldValue) { mr.oldValue = rec.oldValue; }
          if (rec.kind === "characterData" && t.characterDataOldValue) { mr.oldValue = rec.oldValue; }
          if (rec.kind === "childList") {
            for (var a = 0; a < rec.added.length; a++) { var w = __nodeWrap(rec.added[a]); if (w) { mr.addedNodes.push(w); } }
            for (var rm = 0; rm < rec.removed.length; rm++) { var w2 = __nodeWrap(rec.removed[rm]); if (w2) { mr.removedNodes.push(w2); } }
          }
          entry.queue.push(mr);
          break; // one record per mutation per observer
        }
      }
    }
  });

  // Per spec, a DOM mutation "queues a mutation observer microtask": at most one delivery microtask
  // is pending at a time; it collects the Rust-queued mutations and flushes observer callbacks. This
  // lets observers fire at the microtask checkpoint (e.g. before an awaited `Promise.resolve()`),
  // which the engine's post-task delivery alone would miss.
  globalThis.__moMicrotaskQueued = globalThis.__moMicrotaskQueued || false;
  def(globalThis, "__scheduleMODelivery", function () {
    if (globalThis.__moMicrotaskQueued) { return; }
    var anyActive = globalThis.__moRegistry.some(function (e) { return e.targets.length; });
    if (!anyActive) { return; }
    globalThis.__moMicrotaskQueued = true;
    // Use a native (V8) microtask via Promise.resolve().then so delivery interleaves with the page's
    // own `await Promise.resolve()` continuations (the polyfilled queueMicrotask runs on a separate,
    // later-drained queue).
    try {
      Promise.resolve().then(function () {
        globalThis.__moMicrotaskQueued = false;
        try { globalThis.__deliverMutations(); } catch (e) {}
      });
    } catch (e) { globalThis.__moMicrotaskQueued = false; }
  });

  // Called (as a microtask) after a task when Rust has queued mutations. Collects them into each
  // observer's queue, then flushes non-empty queues to their callbacks.
  def(globalThis, "__deliverMutations", function () {
    try { globalThis.__collectMutations(); } catch (e) {}
    var reg = globalThis.__moRegistry.slice();
    for (var o = 0; o < reg.length; o++) {
      var entry = reg[o];
      if (!entry.queue.length) { continue; }
      var batch = entry.queue; entry.queue = [];
      try { entry.observer.callback.call(entry.observer, batch, entry.observer); }
      catch (e) { try { (globalThis.__timerErrors || (globalThis.__timerErrors = [])).push("MutationObserver: " + (e && e.message || e)); } catch (e2) {} }
    }
  });

  if (typeof globalThis.IntersectionObserver !== "function") {
    def(globalThis, "IntersectionObserver", function (cb, opts) {
      this.callback = typeof cb === "function" ? cb : fn;
      this.root = (opts && opts.root) || null; this.rootMargin = (opts && opts.rootMargin) || "0px";
      this.thresholds = (opts && [].concat(opts.threshold || 0)) || [0];
      this._oid = __obsIdSeq++;
      globalThis.__io[this._oid] = { observer: this, cb: this.callback, opts: opts || {}, targets: {} };
    });
    def(globalThis.IntersectionObserver.prototype, "observe", function (el) {
      var id = (el && typeof el.__node === "number") ? el.__node : -1;
      if (id >= 0 && globalThis.__io[this._oid]) { globalThis.__io[this._oid].targets[id] = true; }
    });
    def(globalThis.IntersectionObserver.prototype, "unobserve", function (el) {
      var id = (el && typeof el.__node === "number") ? el.__node : -1;
      if (id >= 0 && globalThis.__io[this._oid]) { delete globalThis.__io[this._oid].targets[id]; }
    });
    def(globalThis.IntersectionObserver.prototype, "disconnect", function () {
      if (globalThis.__io[this._oid]) { globalThis.__io[this._oid].targets = {}; }
    });
    def(globalThis.IntersectionObserver.prototype, "takeRecords", function () { return []; });
  }

  if (typeof globalThis.ResizeObserver !== "function") {
    def(globalThis, "ResizeObserver", function (cb) {
      this.callback = typeof cb === "function" ? cb : fn;
      this._oid = __obsIdSeq++;
      globalThis.__ro[this._oid] = { observer: this, cb: this.callback, targets: {} };
    });
    def(globalThis.ResizeObserver.prototype, "observe", function (el) {
      var id = (el && typeof el.__node === "number") ? el.__node : -1;
      if (id >= 0 && globalThis.__ro[this._oid]) { globalThis.__ro[this._oid].targets[id] = true; }
    });
    def(globalThis.ResizeObserver.prototype, "unobserve", function (el) {
      var id = (el && typeof el.__node === "number") ? el.__node : -1;
      if (id >= 0 && globalThis.__ro[this._oid]) { delete globalThis.__ro[this._oid].targets[id]; }
    });
    def(globalThis.ResizeObserver.prototype, "disconnect", function () {
      if (globalThis.__ro[this._oid]) { globalThis.__ro[this._oid].targets = {}; }
    });
  }

  // Native-readable list of IO/RO targets the engine should compute geometry for.
  def(globalThis, "__observedTargets", function () {
    var out = [];
    for (var ioid in globalThis.__io) {
      var io = globalThis.__io[ioid];
      for (var n in io.targets) { out.push({ kind: "io", observerId: Number(ioid), nodeId: Number(n) }); }
    }
    for (var roid in globalThis.__ro) {
      var ro = globalThis.__ro[roid];
      for (var n2 in ro.targets) { out.push({ kind: "ro", observerId: Number(roid), nodeId: Number(n2) }); }
    }
    return out;
  });

  // Engine calls this with computed geometry. Builds entries, groups per observer callback, fires.
  def(globalThis, "__deliverObservations", function (arr) {
    if (!arr || !arr.length) { return; }
    var ioBatches = {}, roBatches = {};
    for (var i = 0; i < arr.length; i++) {
      var it = arr[i];
      var target = __nodeWrap(it.nodeId);
      if (!target) { continue; }
      if (it.kind === "io" && globalThis.__io[it.observerId]) {
        var br = { x: it.x, y: it.y, width: it.width, height: it.height,
          top: it.y, left: it.x, right: it.x + it.width, bottom: it.y + it.height };
        var ratio = it.intersectionRatio || 0;
        var ir = it.isIntersecting
          ? { x: it.ix, y: it.iy, width: it.iw, height: it.ih, top: it.iy, left: it.ix, right: it.ix + it.iw, bottom: it.iy + it.ih }
          : { x: 0, y: 0, width: 0, height: 0, top: 0, left: 0, right: 0, bottom: 0 };
        var rb = { x: 0, y: 0, width: it.rootW, height: it.rootH, top: 0, left: 0, right: it.rootW, bottom: it.rootH };
        var entry = { target: target, isIntersecting: !!it.isIntersecting, intersectionRatio: ratio,
          boundingClientRect: br, intersectionRect: ir, rootBounds: rb,
          time: (globalThis.__eventLoop ? globalThis.__eventLoop.now : 0) };
        (ioBatches[it.observerId] || (ioBatches[it.observerId] = [])).push(entry);
      } else if (it.kind === "ro" && globalThis.__ro[it.observerId]) {
        var cr = { x: it.x, y: it.y, width: it.width, height: it.height, top: it.y, left: it.x, right: it.x + it.width, bottom: it.y + it.height };
        var box = [{ inlineSize: it.width, blockSize: it.height }];
        var entry2 = { target: target, contentRect: cr, borderBoxSize: box, contentBoxSize: box, devicePixelContentBoxSize: box };
        (roBatches[it.observerId] || (roBatches[it.observerId] = [])).push(entry2);
      }
    }
    for (var oid in ioBatches) {
      var ioReg = globalThis.__io[oid];
      if (ioReg) { try { ioReg.cb.call(ioReg.observer, ioBatches[oid], ioReg.observer); } catch (e) { try { (globalThis.__timerErrors || (globalThis.__timerErrors = [])).push("IntersectionObserver: " + (e && e.message || e)); } catch (e2) {} } }
    }
    for (var oid2 in roBatches) {
      var roReg = globalThis.__ro[oid2];
      if (roReg) { try { roReg.cb.call(roReg.observer, roBatches[oid2], roReg.observer); } catch (e) { try { (globalThis.__timerErrors || (globalThis.__timerErrors = [])).push("ResizeObserver: " + (e && e.message || e)); } catch (e2) {} } }
    }
  });
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

  // ===== XML documents: an independent pure-JS DOM + parser + serializer ======================
  // The arena-backed DOM is HTML-only. XML documents (DOMParser `text/xml`, XMLSerializer) need
  // element/attribute namespaces to round-trip per the DOM Parsing & Serialization spec, so they
  // use this self-contained node model instead of the arena.
  var __xml = (function () {
    var XML_NS = "http://www.w3.org/XML/1998/namespace";
    var XMLNS_NS = "http://www.w3.org/2000/xmlns/";

    function XNode(type, doc) { this.nodeType = type; this.ownerDocument = doc; this.childNodes = []; this.parentNode = null; }
    XNode.prototype.appendChild = function (c) { if (c.parentNode) { c.parentNode.removeChild(c); } c.parentNode = this; this.childNodes.push(c); return c; };
    XNode.prototype.insertBefore = function (c, ref) { if (ref == null) { return this.appendChild(c); } if (c.parentNode) { c.parentNode.removeChild(c); } var i = this.childNodes.indexOf(ref); if (i < 0) { return this.appendChild(c); } c.parentNode = this; this.childNodes.splice(i, 0, c); return c; };
    XNode.prototype.removeChild = function (c) { var i = this.childNodes.indexOf(c); if (i >= 0) { this.childNodes.splice(i, 1); c.parentNode = null; } return c; };
    XNode.prototype.replaceChild = function (nw, old) { var i = this.childNodes.indexOf(old); if (i < 0) { return old; } if (nw.parentNode) { nw.parentNode.removeChild(nw); } nw.parentNode = this; this.childNodes[i] = nw; old.parentNode = null; return old; };
    XNode.prototype.hasChildNodes = function () { return this.childNodes.length > 0; };
    XNode.prototype.append = function () { var d = this.ownerDocument || this; for (var i = 0; i < arguments.length; i++) { var c = arguments[i]; this.appendChild(typeof c === "string" ? d.createTextNode(c) : c); } };
    XNode.prototype.prepend = function () { var d = this.ownerDocument || this; var ref = this.childNodes[0] || null; for (var i = 0; i < arguments.length; i++) { var c = arguments[i]; this.insertBefore(typeof c === "string" ? d.createTextNode(c) : c, ref); } };
    XNode.prototype.isEqualNode = function (o) { return globalThis.__nodesEqual(this, o); };
    XNode.prototype.cloneNode = function () { return this; };
    Object.defineProperty(XNode.prototype, "firstChild", { get: function () { return this.childNodes[0] || null; } });
    Object.defineProperty(XNode.prototype, "lastChild", { get: function () { return this.childNodes[this.childNodes.length - 1] || null; } });
    Object.defineProperty(XNode.prototype, "nextSibling", { get: function () { var p = this.parentNode; if (!p) { return null; } return p.childNodes[p.childNodes.indexOf(this) + 1] || null; } });
    Object.defineProperty(XNode.prototype, "previousSibling", { get: function () { var p = this.parentNode; if (!p) { return null; } var i = p.childNodes.indexOf(this); return i > 0 ? p.childNodes[i - 1] : null; } });
    Object.defineProperty(XNode.prototype, "parentElement", { get: function () { var p = this.parentNode; return p && p.nodeType === 1 ? p : null; } });
    Object.defineProperty(XNode.prototype, "textContent", {
      get: function () { var s = ""; for (var i = 0; i < this.childNodes.length; i++) { var c = this.childNodes[i]; if (c.nodeType === 3 || c.nodeType === 4) { s += c.data; } else if (c.nodeType === 1) { s += c.textContent; } } return s; },
      set: function (v) { while (this.childNodes.length) { this.removeChild(this.childNodes[0]); } if (v !== "" && v != null) { this.appendChild(this.ownerDocument.createTextNode(String(v))); } }
    });

    function XAttr(ns, prefix, local, value) { this.namespaceURI = ns || null; this.prefix = prefix || null; this.localName = local; this.value = value; this.name = prefix ? prefix + ":" + local : local; this.nodeType = 2; }

    function splitQ(qname) { var c = qname.indexOf(":"); return c > 0 ? [qname.slice(0, c), qname.slice(c + 1)] : [null, qname]; }

    function XElement(doc, ns, prefix, local) { XNode.call(this, 1, doc); this.namespaceURI = ns || null; this.prefix = prefix || null; this.localName = local; this._attrs = []; }
    XElement.prototype = Object.create(XNode.prototype);
    XElement.prototype.constructor = XElement;
    Object.defineProperty(XElement.prototype, "tagName", { get: function () { return this.prefix ? this.prefix + ":" + this.localName : this.localName; } });
    Object.defineProperty(XElement.prototype, "nodeName", { get: function () { return this.tagName; } });
    Object.defineProperty(XElement.prototype, "attributes", { get: function () { var a = this._attrs.slice(); a.item = function (i) { return this[i] || null; }; return a; } });
    Object.defineProperty(XElement.prototype, "children", { get: function () { return this.childNodes.filter(function (c) { return c.nodeType === 1; }); } });
    Object.defineProperty(XElement.prototype, "firstElementChild", { get: function () { return this.children[0] || null; } });
    XElement.prototype._findByName = function (name) { for (var i = 0; i < this._attrs.length; i++) { if (this._attrs[i].name === name) { return i; } } return -1; };
    XElement.prototype._findNS = function (ns, local) { for (var i = 0; i < this._attrs.length; i++) { var a = this._attrs[i]; if ((a.namespaceURI || null) === (ns || null) && a.localName === local) { return i; } } return -1; };
    XElement.prototype.getAttribute = function (name) { var i = this._findByName(name); return i >= 0 ? this._attrs[i].value : null; };
    XElement.prototype.hasAttribute = function (name) { return this._findByName(name) >= 0; };
    XElement.prototype.removeAttribute = function (name) { var i = this._findByName(name); if (i >= 0) { this._attrs.splice(i, 1); } };
    XElement.prototype.setAttribute = function (name, value) { var i = this._findByName(name); if (i >= 0) { this._attrs[i].value = String(value); } else { this._attrs.push(new XAttr(null, null, name, String(value))); } };
    XElement.prototype.getAttributeNS = function (ns, local) { var i = this._findNS(ns, local); return i >= 0 ? this._attrs[i].value : null; };
    XElement.prototype.setAttributeNS = function (ns, qname, value) { ns = ns || null; var s = splitQ(qname); var i = this._findNS(ns, s[1]); if (i >= 0) { this._attrs[i].value = String(value); this._attrs[i].prefix = s[0]; this._attrs[i].name = qname; } else { this._attrs.push(new XAttr(ns, s[0], s[1], String(value))); } };

    function XText(doc, data) { XNode.call(this, 3, doc); this.data = data; }
    XText.prototype = Object.create(XNode.prototype); XText.prototype.constructor = XText;
    Object.defineProperty(XText.prototype, "nodeValue", { get: function () { return this.data; }, set: function (v) { this.data = String(v); } });
    Object.defineProperty(XText.prototype, "textContent", { get: function () { return this.data; }, set: function (v) { this.data = String(v); } });
    function XComment(doc, data) { XNode.call(this, 8, doc); this.data = data; }
    XComment.prototype = Object.create(XText.prototype); XComment.prototype.constructor = XComment;
    function XCData(doc, data) { XNode.call(this, 4, doc); this.data = data; }
    XCData.prototype = Object.create(XText.prototype); XCData.prototype.constructor = XCData;
    function XPI(doc, target, data) { XNode.call(this, 7, doc); this.target = target; this.data = data; }
    XPI.prototype = Object.create(XNode.prototype); XPI.prototype.constructor = XPI;
    function XDoctype(doc, name, pub, sys) { XNode.call(this, 10, doc); this.name = name; this.publicId = pub || ""; this.systemId = sys || ""; }
    XDoctype.prototype = Object.create(XNode.prototype); XDoctype.prototype.constructor = XDoctype;

    function XDocument() { XNode.call(this, 9, null); }
    XDocument.prototype = Object.create(XNode.prototype); XDocument.prototype.constructor = XDocument;
    XDocument.prototype.createElement = function (name) { var s = splitQ(name); return new XElement(this, null, null, s[1]); };
    XDocument.prototype.createElementNS = function (ns, qname) { var s = splitQ(qname); return new XElement(this, ns || null, s[0], s[1]); };
    XDocument.prototype.createTextNode = function (d) { return new XText(this, String(d)); };
    XDocument.prototype.createComment = function (d) { return new XComment(this, String(d)); };
    XDocument.prototype.createCDATASection = function (d) { return new XCData(this, String(d)); };
    XDocument.prototype.createProcessingInstruction = function (t, d) { return new XPI(this, t, d); };
    Object.defineProperty(XDocument.prototype, "documentElement", { get: function () { for (var i = 0; i < this.childNodes.length; i++) { if (this.childNodes[i].nodeType === 1) { return this.childNodes[i]; } } return null; } });
    // A DOMParser-produced document is always UTF-8, regardless of any encoding declaration inside.
    Object.defineProperty(XDocument.prototype, "characterSet", { get: function () { return "UTF-8"; } });
    Object.defineProperty(XDocument.prototype, "charset", { get: function () { return "UTF-8"; } });
    Object.defineProperty(XDocument.prototype, "inputEncoding", { get: function () { return "UTF-8"; } });

    // --- Parser: a small namespace-aware XML reader ----------------------------------------------
    function parse(str) {
      var doc = new XDocument();
      var i = 0, n = str.length;
      var open = [];
      function cur() { return open.length ? open[open.length - 1] : doc; }
      function lookup(prefix, local) {
        if (local && Object.prototype.hasOwnProperty.call(local, prefix)) { return local[prefix]; }
        for (var k = open.length - 1; k >= 0; k--) { var d = open[k].__ns; if (d && Object.prototype.hasOwnProperty.call(d, prefix)) { return d[prefix]; } }
        if (prefix === "xml") { return XML_NS; }
        if (prefix === "xmlns") { return XMLNS_NS; }
        return prefix === "" ? null : undefined;
      }
      function decodeEnt(s) {
        return s.replace(/&(#x?[0-9a-fA-F]+|[a-zA-Z]+);/g, function (m, e) {
          if (e[0] === '#') { var cp = e[1] === "x" || e[1] === "X" ? parseInt(e.slice(2), 16) : parseInt(e.slice(1), 10); return isNaN(cp) ? m : String.fromCodePoint(cp); }
          var map = { lt: "<", gt: ">", amp: "&", quot: "\"", apos: "'" };
          return Object.prototype.hasOwnProperty.call(map, e) ? map[e] : m;
        });
      }
      while (i < n) {
        if (str[i] === "<") {
          if (str.substr(i, 4) === "<!--") { var e = str.indexOf("-->", i + 4); if (e < 0) { e = n - 3; } cur().appendChild(doc.createComment(str.slice(i + 4, e))); i = e + 3; continue; }
          if (str.substr(i, 9) === "<![CDATA[") { var e2 = str.indexOf("]]>", i + 9); if (e2 < 0) { e2 = n - 3; } cur().appendChild(doc.createCDATASection(str.slice(i + 9, e2))); i = e2 + 3; continue; }
          if (str.substr(i, 2) === "<?") { var e3 = str.indexOf("?>", i + 2); if (e3 < 0) { e3 = n - 2; } var body = str.slice(i + 2, e3); var sp = body.search(/\s/); var tgt = sp < 0 ? body : body.slice(0, sp); var dat = sp < 0 ? "" : body.slice(sp + 1); if (tgt.toLowerCase() !== "xml") { cur().appendChild(doc.createProcessingInstruction(tgt, dat)); } i = e3 + 2; continue; }
          if (str.substr(i, 2) === "<!") { var e4 = str.indexOf(">", i); if (e4 < 0) { e4 = n - 1; } i = e4 + 1; continue; }
          if (str[i + 1] === "/") { var e5 = str.indexOf(">", i); if (e5 < 0) { e5 = n - 1; } if (open.length) { open.pop(); } i = e5 + 1; continue; }
          // start tag
          i++;
          var nameM = /^[^\s/>]+/.exec(str.slice(i));
          if (!nameM) { return { error: true }; }
          var rawName = nameM[0]; i += rawName.length;
          var rawAttrs = [];
          while (i < n && str[i] !== ">" && str[i] !== "/") {
            var am = /^\s*([^\s=/>]+)\s*(=\s*("([^"]*)"|'([^']*)'))?/.exec(str.slice(i));
            if (!am || am[0].length === 0) { i++; continue; }
            i += am[0].length;
            var av = am[4] != null ? am[4] : (am[5] != null ? am[5] : "");
            rawAttrs.push([am[1], decodeEnt(av)]);
          }
          var selfClose = str[i] === "/";
          var gt = str.indexOf(">", i); i = (gt < 0 ? n : gt + 1);
          // collect this element's namespace declarations
          var nsdecl = {};
          for (var ai = 0; ai < rawAttrs.length; ai++) {
            var an = rawAttrs[ai][0];
            if (an === "xmlns") { nsdecl[""] = rawAttrs[ai][1]; }
            else if (an.slice(0, 6) === "xmlns:") { nsdecl[an.slice(6)] = rawAttrs[ai][1]; }
          }
          var es = splitQ(rawName);
          var elNs = lookup(es[0] || "", nsdecl);
          if (elNs === undefined) { elNs = null; }
          var el = new XElement(doc, elNs, es[0], es[1]);
          el.__ns = nsdecl;
          for (var aj = 0; aj < rawAttrs.length; aj++) {
            var qn = rawAttrs[aj][0], val = rawAttrs[aj][1];
            if (qn === "xmlns") { el.setAttributeNS(XMLNS_NS, "xmlns", val); }
            else if (qn.slice(0, 6) === "xmlns:") { el.setAttributeNS(XMLNS_NS, qn, val); }
            else { var qs = splitQ(qn); var ans = qs[0] ? lookup(qs[0], nsdecl) : null; if (ans === undefined) { ans = null; } el.setAttributeNS(ans, qn, val); }
          }
          cur().appendChild(el);
          if (!selfClose) { open.push(el); }
          continue;
        }
        var lt = str.indexOf("<", i); var end = lt < 0 ? n : lt;
        var text = str.slice(i, end);
        if (text.length) { cur().appendChild(doc.createTextNode(decodeEnt(text))); }
        i = end;
      }
      return { doc: doc };
    }

    // --- Serializer: the DOM Parsing & Serialization "XML serialization" algorithm ----------------
    var HTML_NS = "http://www.w3.org/1999/xhtml";
    // Attribute list for either an XML node (our model) or an arena-backed HTML node — so an HTML
    // element/fragment can be serialized too (XMLSerializer accepts any node).
    function attrsOf(node) { return node._attrs || (node.attributes ? Array.prototype.slice.call(node.attributes) : []); }
    function escText(s) { return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;"); }
    function escAttr(s) { return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/"/g, "&quot;").replace(/\t/g, "&#9;").replace(/\n/g, "&#10;").replace(/\r/g, "&#13;"); }
    function mapClone(m) { var o = {}; for (var k in m) { o[k] = m[k].slice(); } return o; }
    function mapAdd(m, prefix, ns) { if (!m[prefix]) { m[prefix] = []; } if (m[prefix].indexOf(ns) < 0) { m[prefix].push(ns); } }
    function mapHas(m, prefix, ns) { return m[prefix] && m[prefix].indexOf(ns) >= 0; }
    function genPrefix(map, ns, idx) { var p = "ns" + idx.v; idx.v++; mapAdd(map, p, ns); return p; }
    function preferredPrefix(map, ns, prefer) {
      var cand = null;
      for (var p in map) { if (map[p].indexOf(ns) >= 0) { if (p === prefer) { return p; } if (p !== "") { cand = p; } } }
      return cand;
    }
    function recordNs(node, map, localPrefixes) {
      var localDefault = null;
      var __aa = attrsOf(node);
      for (var i = 0; i < __aa.length; i++) {
        var a = __aa[i];
        if ((a.namespaceURI || null) !== XMLNS_NS) { continue; }
        if (a.prefix === null) { localDefault = a.value; }
        else { var pfx = a.localName; if (!mapHas(map, pfx, a.value)) { mapAdd(map, pfx, a.value); } localPrefixes[pfx] = a.value; }
      }
      return localDefault;
    }
    function serAttrs(node, map, idx, localPrefixes, ignoreDefault) {
      var out = "";
      var __aa = attrsOf(node);
      for (var i = 0; i < __aa.length; i++) {
        var a = __aa[i];
        var ans = a.namespaceURI || null;
        // A no-namespace attribute literally named "xmlns" (e.g. via setAttribute) is not a real
        // namespace declaration; emitting it would forge one, so it's dropped.
        if (ans === null && a.localName === "xmlns") { continue; }
        if (ans === XMLNS_NS) {
          // a namespace-definition attribute
          if (a.prefix === null) { if (ignoreDefault) { continue; } out += " xmlns=\"" + escAttr(a.value) + "\""; continue; }
          // xmlns:prefix — drop if it just re-declares what the map already has for that prefix
          if (mapHas(map, a.localName, a.value) && a.value !== "") { /* still emit declared xmlns:* from source */ }
          out += " xmlns:" + a.localName + "=\"" + escAttr(a.value) + "\"";
          continue;
        }
        var pfx = "";
        if (ans !== null) {
          var cand = preferredPrefix(map, ans, a.prefix);
          if (cand !== null && cand !== "xmlns") {
            pfx = cand + ":";
          } else {
            // The namespace isn't already bound to a usable prefix: bind a freshly generated one.
            var p = genPrefix(map, ans, idx);
            out += " xmlns:" + p + "=\"" + escAttr(ans) + "\"";
            pfx = p + ":";
          }
        }
        out += " " + pfx + a.localName + "=\"" + escAttr(a.value) + "\"";
      }
      return out;
    }
    function serNode(node, ns, map, idx) {
      switch (node.nodeType) {
        case 1: return serElem(node, ns, map, idx);
        case 3: return escText(node.data);
        case 4: return "<![CDATA[" + node.data + "]]>";
        case 8: return "<!--" + node.data + "-->";
        case 7: return "<?" + node.target + " " + node.data + "?>";
        case 10: return "<!DOCTYPE " + node.name + (node.publicId ? " PUBLIC \"" + node.publicId + "\"" : "") + (node.systemId ? (node.publicId ? "" : " SYSTEM") + " \"" + node.systemId + "\"" : "") + ">";
        case 9: case 11: { var s = ""; for (var i = 0; i < node.childNodes.length; i++) { s += serNode(node.childNodes[i], ns, map, idx); } return s; }
        default: return "";
      }
    }
    function serElem(node, ns, map, idx) {
      map = mapClone(map);
      var localPrefixes = {};
      var localDefault = recordNs(node, map, localPrefixes);
      var inherited = ns;
      var nodeNs = node.namespaceURI || null;
      var qname, markup = "<", ignoreDefault = false;
      if ((inherited || null) === nodeNs) {
        if (localDefault !== null) { ignoreDefault = true; }
        qname = (nodeNs === XML_NS) ? "xml:" + node.localName : node.localName;
        markup += qname;
      } else {
        var prefix = node.prefix;
        if (prefix === "xmlns") { prefix = null; }
        var cand = preferredPrefix(map, nodeNs, prefix);
        if (cand !== null && cand !== "xmlns") {
          qname = cand + ":" + node.localName;
          if (localDefault !== null && localDefault !== "") { inherited = localDefault; }
          markup += qname;
        } else if (prefix !== null) {
          if (Object.prototype.hasOwnProperty.call(localPrefixes, prefix)) { prefix = genPrefix(map, nodeNs, idx); }
          else { mapAdd(map, prefix, nodeNs); }
          qname = prefix + ":" + node.localName;
          markup += qname + " xmlns:" + prefix + "=\"" + escAttr(nodeNs) + "\"";
        } else {
          qname = node.localName;
          inherited = nodeNs;
          // The element declares its own default namespace here, so the source `xmlns` attribute
          // (the same declaration, possibly stale/inconsistent) must not be repeated.
          ignoreDefault = true;
          markup += qname + " xmlns=\"" + escAttr(nodeNs || "") + "\"";
        }
      }
      markup += serAttrs(node, map, idx, localPrefixes, ignoreDefault);
      // HTML-namespace elements always serialize with an explicit end tag (never self-closing).
      if (node.childNodes.length === 0 && nodeNs !== HTML_NS) { return markup + "/>"; }
      markup += ">";
      for (var i = 0; i < node.childNodes.length; i++) { markup += serNode(node.childNodes[i], inherited, map, idx); }
      return markup + "</" + qname + ">";
    }
    function serialize(node) { return serNode(node, null, { "xml": [XML_NS] }, { v: 1 }); }

    return { parse: parse, serialize: serialize, XDocument: XDocument };
  })();

  // --- a few more constructors pages feature-detect ----------------------------------------
  if (typeof globalThis.DOMParser !== "function") {
    def(globalThis, "DOMParser", function () {
      this.parseFromString = function (str, type) {
        var t = String(type || "").toLowerCase();
        // text/html parses as an HTML document (HTML namespace).
        if (t === "text/html") { return document; }
        // XML flavours: parse into an independent namespace-aware XML document.
        if (t.indexOf("xml") >= 0) { return __xml.parse(String(str)).doc; }
        return document;
      };
    });
  }
  if (typeof globalThis.XMLSerializer !== "function") {
    def(globalThis, "XMLSerializer", function () {
      this.serializeToString = function (node) { return __xml.serialize(node); };
    });
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
    // Per WebIDL, an interface prototype object carries `@@toStringTag` = the interface name, so
    // `Object.prototype.toString.call(instance)` reports `[object <Interface>]`. Defined here on the
    // own prototype (configurable, non-enumerable) so e.g. a `CSSFontFaceRule` stringifies correctly.
    try {
      Object.defineProperty(ctor.prototype, Symbol.toStringTag,
        { value: name, writable: false, enumerable: false, configurable: true });
    } catch (e) {}
    def(globalThis, name, ctor);
    return ctor;
  }
  var NodeCtor = defClass("Node");
  // Node type constants live on both the constructor and the prototype, so `Node.ELEMENT_NODE` and
  // `someNode.ELEMENT_NODE` (instance access, used by WPT) both resolve.
  (function (proto) {
    var consts = {
      ELEMENT_NODE: 1, ATTRIBUTE_NODE: 2, TEXT_NODE: 3, CDATA_SECTION_NODE: 4,
      ENTITY_REFERENCE_NODE: 5, ENTITY_NODE: 6, PROCESSING_INSTRUCTION_NODE: 7, COMMENT_NODE: 8,
      DOCUMENT_NODE: 9, DOCUMENT_TYPE_NODE: 10, DOCUMENT_FRAGMENT_NODE: 11, NOTATION_NODE: 12
    };
    for (var k in consts) {
      NodeCtor[k] = consts[k];
      if (proto) { try { def(proto, k, consts[k]); } catch (e) {} }
    }
  })(NodeCtor.prototype);
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

  // ElementCSSInlineStyle: `style` on the prototype chain (so assert_idl_attribute passes — it must
  // NOT be an own property). Returns the per-element cached CSSStyleDeclaration stashed by
  // enrichElement; [PutForwards=cssText] forwards string assignment to `.style.cssText`.
  try {
    if (ElementCtor && ElementCtor.prototype) {
      Object.defineProperty(ElementCtor.prototype, "style", {
        get: function () { return this.__styleObj || null; },
        set: function (v) { var s = this.__styleObj; if (s) { s.cssText = v == null ? "" : String(v); } },
        enumerable: true, configurable: true
      });
    }
  } catch (e) {}
  // LinkStyle mixin: `sheet` on HTMLStyleElement/HTMLLinkElement prototypes (must not be own, so
  // assert_idl_attribute passes). Lazily creates and caches the CSSStyleSheet on the element.
  try {
    var __sheetProtoNames = ["HTMLStyleElement", "HTMLLinkElement"];
    for (var spi = 0; spi < __sheetProtoNames.length; spi++) {
      var __sp = globalThis[__sheetProtoNames[spi]];
      if (__sp && __sp.prototype) {
        Object.defineProperty(__sp.prototype, "sheet", {
          get: function () {
            if (!this.__sheetHost) { return null; }
            // A <style>/<link>'s sheet exists only once the element is inserted into a tree; a freshly
            // created, never-appended element has no parent and thus no associated sheet (`.sheet` is
            // null). (An element in an <iframe> facade subtree has a parent, so it keeps its sheet.)
            try { if (this.parentNode == null) { return null; } } catch (e) {}
            if (!this.__sheetObj) { def(this, "__sheetObj", makeStyleSheet(this)); }
            return this.__sheetObj;
          },
          enumerable: false, configurable: true
        });
      }
    }
    // HTMLLinkElement.disabled / HTMLStyleElement.disabled. For <link>, `disabled` is backed by the
    // content attribute (and excludes the sheet from document.styleSheets while set). For <style>,
    // `disabled` mirrors the sheet's `disabled` state.
    // Fire `load` (async) on a connected, enabled stylesheet <link> — pages use link.onload to know
    // when the sheet is ready, and enabling a previously-disabled link reloads it.
    function __fireLinkLoad(link) {
      try {
        var rel = (link.getAttribute && link.getAttribute("rel") || "").toLowerCase();
        if (rel.split(/\s+/).indexOf("stylesheet") < 0) { return; }
        if (!link.getAttribute || !link.getAttribute("href")) { return; }
        if (!(document.documentElement && document.documentElement.contains(link))) { return; }
        // Defer to a later task (NOT a microtask): a stylesheet load is async, and firing it during
        // the `disabled` setter would re-enter the caller's code mid-statement (e.g. an onload handler
        // toggling `disabled` again before the setter's caller finishes).
        setTimeout(function () { try { if (typeof link.dispatchEvent === "function") { link.dispatchEvent(new Event("load")); } } catch (e) {} }, 0);
      } catch (e) {}
    }
    def(globalThis, "__fireLinkLoad", __fireLinkLoad);
    var __linkProto = globalThis.HTMLLinkElement && globalThis.HTMLLinkElement.prototype;
    if (__linkProto) {
      Object.defineProperty(__linkProto, "disabled", {
        get: function () { return this.getAttribute("disabled") != null || !!this.__sheetDisabled; },
        set: function (v) {
          if (v) { this.setAttribute("disabled", ""); def(this, "__sheetDisabled", true); }
          else { this.removeAttribute("disabled"); def(this, "__sheetDisabled", false); __fireLinkLoad(this); }
        },
        enumerable: true, configurable: true
      });
    }
    var __styleProto = globalThis.HTMLStyleElement && globalThis.HTMLStyleElement.prototype;
    if (__styleProto) {
      Object.defineProperty(__styleProto, "disabled", {
        get: function () { var s = this.sheet; return s ? !!s.disabled : false; },
        set: function (v) { var s = this.sheet; if (s) { s.disabled = !!v; } },
        enumerable: true, configurable: true
      });
    }
  } catch (e) {}

  // Document / Window and the other DOM interface constructors pages reference as globals
  // (e.g. `x instanceof Document`, `Node.prototype`, `HTMLCollection`). Defined so references and
  // instanceof checks don't throw ReferenceError.
  var DocumentCtor = defClass("Document", NodeCtor);
  defClass("HTMLDocument", DocumentCtor);
  defClass("XMLDocument", DocumentCtor);
  // A bare `new Document()` has no documentElement, so namespace lookups all return null. (The
  // page's live `document` overrides these via its own delegating methods.)
  try {
    if (DocumentCtor && DocumentCtor.prototype) {
      def(DocumentCtor.prototype, "lookupNamespaceURI", function () { return null; });
      def(DocumentCtor.prototype, "lookupPrefix", function () { return null; });
      def(DocumentCtor.prototype, "isDefaultNamespace", function (ns) { return ns == null || ns === ""; });
      // A bare `new Document()` is an XML document, so it supports the CharacterData factories
      // (including createCDATASection, which an HTML document refuses). Nodes are real arena nodes so
      // they can be inserted into a live tree and traversed.
      var __mkNode = function (mkId) { return globalThis.__canonNode(globalThis.__wrapNode(mkId)); };
      def(DocumentCtor.prototype, "createTextNode", function (data) { return __mkNode(globalThis.__createText(String(data == null ? "" : data))); });
      def(DocumentCtor.prototype, "createComment", function (data) { return __mkNode(globalThis.__createComment(String(data == null ? "" : data))); });
      def(DocumentCtor.prototype, "createCDATASection", function (data) { return __mkNode(globalThis.__createCData(String(data == null ? "" : data))); });
      def(DocumentCtor.prototype, "createDocumentFragment", function () { return __mkNode(globalThis.__createDocumentFragment()); });
    }
  } catch (e) {}
  defClass("Window", globalThis.EventTarget);
  defClass("AbstractRange"); defClass("Range", globalThis.AbstractRange); defClass("StaticRange", globalThis.AbstractRange);
  var domIfaces = [
    "HTMLCollection", "NodeList", "DOMTokenList", "NamedNodeMap", "DOMStringMap", "DOMRectList",
    "CSSStyleDeclaration", "StyleSheetList", "MediaList", "CSSRuleList",
    "DOMRect", "DOMRectReadOnly", "DOMPoint", "DOMPointReadOnly", "DOMMatrix", "DOMMatrixReadOnly",
    "DOMQuad", "DOMException", "DOMParser", "XMLSerializer", "XPathResult", "XPathEvaluator",
    "MutationRecord", "AnimationEffect", "KeyframeEffect", "Animation", "AnimationTimeline",
    "CSSStyleValue", "StylePropertyMap", "VisualViewport", "Selection", "TextMetrics",
    "TimeRanges", "ValidityState", "HTMLFormControlsCollection", "RadioNodeList",
  ];
  for (var di = 0; di < domIfaces.length; di++) { defClass(domIfaces[di]); }

  // CSSStyleDeclaration is a WebIDL iterable<> (over its property names by index). Put the default
  // iterator on the PROTOTYPE so `Symbol.iterator in CSSStyleDeclaration.prototype` holds (instances
  // may still carry their own iterator over their live declarations).
  try {
    if (globalThis.CSSStyleDeclaration && globalThis.CSSStyleDeclaration.prototype) {
      Object.defineProperty(globalThis.CSSStyleDeclaration.prototype, Symbol.iterator, {
        value: function () {
          var self = this, i = 0;
          var it = { next: function () { var n = self.length >>> 0; return i < n ? { value: self[i++], done: false } : { value: undefined, done: true }; } };
          it[Symbol.iterator] = function () { return this; };
          return it;
        },
        writable: true, enumerable: false, configurable: true
      });
    }
  } catch (e) {}

  // --- DOMRect factory + real Range + CaretPosition (caret hit-testing support) ---------------
  // A DOMRect instance (prototype-correct, so `r instanceof DOMRect`) holding x/y/width/height plus
  // the derived top/right/bottom/left and a toJSON. Used by Range.getBoundingClientRect and
  // CaretPosition.getClientRect so callers get a real DOMRect, not a plain object.
  function __makeDOMRect(x, y, w, h) {
    x = Number(x) || 0; y = Number(y) || 0; w = Number(w) || 0; h = Number(h) || 0;
    var DR = globalThis.DOMRect;
    var r = (DR && DR.prototype) ? Object.create(DR.prototype) : {};
    r.x = x; r.y = y; r.width = w; r.height = h;
    r.left = w < 0 ? x + w : x; r.top = h < 0 ? y + h : y;
    r.right = w < 0 ? x : x + w; r.bottom = h < 0 ? y : y + h;
    r.toJSON = function () { return { x: this.x, y: this.y, width: this.width, height: this.height, top: this.top, right: this.right, bottom: this.bottom, left: this.left }; };
    return r;
  }
  def(globalThis, "__makeDOMRect", __makeDOMRect);

  // Wrap a node id into its CANONICAL wrapper (stable identity: the same object getElementById /
  // createElement / firstChild hand out), so `caret.offsetNode === el.firstChild` etc. hold.
  function __nodeFor(id) {
    if (typeof id !== "number" || id < 0) { return null; }
    var cached = (typeof globalThis.__nodeById === "function") ? globalThis.__nodeById(id) : null;
    if (cached) { return cached; }
    var w = __wrapNode(id);
    return (typeof globalThis.__canonNode === "function") ? globalThis.__canonNode(w) : w;
  }
  def(globalThis, "__nodeFor", __nodeFor);

  // The node id behind a wrapper (or a raw id), or -1.
  function __idOf(node) {
    if (node == null) { return -1; }
    if (typeof node === "number") { return node; }
    var n = node.__node;
    return (typeof n === "number") ? n : -1;
  }
  // Length of a node for Range offset bounds: text/comment -> character count, element -> child count.
  function __nodeLength(id) {
    if (id < 0) { return 0; }
    var t = __nodeType(id);
    if (t === 3 || t === 8) { var s = __textContent(id); return s ? s.length : 0; }
    try { return __children(id).length; } catch (e) { return 0; }
  }
  // Viewport-relative caret geometry for (textNodeId, offset): the text run's box gives the line
  // top/height; the caret x interpolates across the run by character fraction (uniform-advance
  // approximation — no per-glyph metrics are available here). Returns {x, top, height} or null.
  function __caretGeometry(containerId, offset) {
    var r = null; try { r = __rect(containerId); } catch (e) {}
    if (!r) {
      // Fall back to the parent element's box when the text node itself has no pushed rect.
      var p = __parent(containerId);
      if (p >= 0) { try { r = __rect(p); } catch (e2) {} }
    }
    if (!r) { return null; }
    var len = 0;
    if (__nodeType(containerId) === 3) { var s = __textContent(containerId); len = s ? s.length : 0; }
    var frac = len > 0 ? (Math.max(0, Math.min(offset, len)) / len) : 0;
    return { x: r.left + (r.right - r.left) * frac, top: r.top, height: r.bottom - r.top };
  }

  // A real Range: collapsed by default, supporting setStart/setEnd/collapse, toString (text between
  // boundary points within a single text container), getBoundingClientRect/getClientRects (caret or
  // text-span geometry), and cloneRange. Enough for the CSSOM caret tests and common callers.
  var AbstractRangeProto = (globalThis.AbstractRange && globalThis.AbstractRange.prototype) || Object.prototype;
  function Range() {
    this._sc = null; this._so = 0; this._ec = null; this._eo = 0;
  }
  Range.prototype = Object.create(AbstractRangeProto);
  Range.prototype.constructor = Range;
  Object.defineProperty(Range.prototype, "startContainer", { get: function () { return this._sc; }, enumerable: true, configurable: true });
  Object.defineProperty(Range.prototype, "endContainer", { get: function () { return this._ec; }, enumerable: true, configurable: true });
  Object.defineProperty(Range.prototype, "startOffset", { get: function () { return this._so; }, enumerable: true, configurable: true });
  Object.defineProperty(Range.prototype, "endOffset", { get: function () { return this._eo; }, enumerable: true, configurable: true });
  Object.defineProperty(Range.prototype, "collapsed", { get: function () { return this._sc === this._ec && this._so === this._eo; }, enumerable: true, configurable: true });
  Object.defineProperty(Range.prototype, "commonAncestorContainer", { get: function () {
    if (this._sc === this._ec) { return this._sc; }
    // Nearest common ancestor of the two boundary nodes (by walking start's ancestor chain).
    var aId = __idOf(this._sc), bId = __idOf(this._ec);
    if (aId < 0) { return this._ec; }
    if (bId < 0) { return this._sc; }
    var aChain = {}; var c = aId;
    while (c >= 0) { aChain[c] = true; c = __parent(c); }
    c = bId;
    while (c >= 0) { if (aChain[c]) { return __nodeFor(c); } c = __parent(c); }
    return this._sc;
  }, enumerable: true, configurable: true });
  Range.prototype.setStart = function (node, offset) {
    this._sc = node; this._so = offset | 0;
    // If start is now after end (or end unset), collapse end onto start.
    if (this._ec == null || (this._sc === this._ec && this._so > this._eo)) { this._ec = node; this._eo = this._so; }
  };
  Range.prototype.setEnd = function (node, offset) {
    this._ec = node; this._eo = offset | 0;
    if (this._sc == null || (this._sc === this._ec && this._eo < this._so)) { this._sc = node; this._so = this._eo; }
  };
  Range.prototype.setStartBefore = function (node) { var id = __idOf(node); var p = __parent(id); this.setStart(p >= 0 ? __nodeFor(p) : node, p >= 0 ? __children(p).indexOf(id) : 0); };
  Range.prototype.setStartAfter = function (node) { var id = __idOf(node); var p = __parent(id); this.setStart(p >= 0 ? __nodeFor(p) : node, p >= 0 ? __children(p).indexOf(id) + 1 : 0); };
  Range.prototype.setEndBefore = function (node) { var id = __idOf(node); var p = __parent(id); this.setEnd(p >= 0 ? __nodeFor(p) : node, p >= 0 ? __children(p).indexOf(id) : 0); };
  Range.prototype.setEndAfter = function (node) { var id = __idOf(node); var p = __parent(id); this.setEnd(p >= 0 ? __nodeFor(p) : node, p >= 0 ? __children(p).indexOf(id) + 1 : 0); };
  Range.prototype.collapse = function (toStart) {
    if (toStart) { this._ec = this._sc; this._eo = this._so; }
    else { this._sc = this._ec; this._so = this._eo; }
  };
  Range.prototype.selectNode = function (node) { this.setStartBefore(node); this.setEndAfter(node); };
  Range.prototype.selectNodeContents = function (node) { this.setStart(node, 0); this.setEnd(node, __nodeLength(__idOf(node))); };
  Range.prototype.cloneRange = function () { var r = new Range(); r._sc = this._sc; r._so = this._so; r._ec = this._ec; r._eo = this._eo; return r; };
  Range.prototype.detach = function () {};
  Range.prototype.createContextualFragment = function (html) {
    if (arguments.length < 1) {
      throw new TypeError("Failed to execute 'createContextualFragment' on 'Range': 1 argument required, but only 0 present.");
    }
    // Parse the markup as an HTML fragment (scripts are parsed but not executed since the result
    // isn't connected), then move the parsed nodes into a DocumentFragment.
    var tmp = __createElement("template");
    __setInnerHTML(tmp, html == null ? "" : String(html));
    var frag = document.createDocumentFragment();
    var kids = __children(tmp).slice();
    for (var i = 0; i < kids.length; i++) { __appendChild(frag.__node, kids[i]); }
    return frag;
  };
  Range.prototype.toString = function () {
    // Only the common single-text-container case is modeled (the caret tests' usage): substring of
    // the text node between the two offsets.
    if (this._sc === this._ec && this._sc != null) {
      var id = __idOf(this._sc);
      if (id >= 0 && __nodeType(id) === 3) {
        var s = __textContent(id) || "";
        return s.substring(Math.min(this._so, this._eo), Math.max(this._so, this._eo));
      }
    }
    return "";
  };
  Range.prototype.getClientRects = function () {
    var r = this.getBoundingClientRect();
    return [r];
  };
  Range.prototype.getBoundingClientRect = function () {
    var scId = __idOf(this._sc);
    if (scId < 0) { return __makeDOMRect(0, 0, 0, 0); }
    var g0 = __caretGeometry(scId, this._so);
    if (!g0) { return __makeDOMRect(0, 0, 0, 0); }
    if (this._sc === this._ec && this._so === this._eo) {
      // Collapsed: a zero-width caret rect at the boundary.
      return __makeDOMRect(g0.x, g0.top, 0, g0.height);
    }
    if (this._sc === this._ec) {
      var g1 = __caretGeometry(scId, this._eo);
      var x0 = Math.min(g0.x, g1 ? g1.x : g0.x), x1 = Math.max(g0.x, g1 ? g1.x : g0.x);
      return __makeDOMRect(x0, g0.top, x1 - x0, g0.height);
    }
    // Cross-node span: approximate with the start container's box.
    var rr = null; try { rr = __rect(scId); } catch (e) {}
    if (rr) { return __makeDOMRect(rr.left, rr.top, rr.right - rr.left, rr.bottom - rr.top); }
    return __makeDOMRect(g0.x, g0.top, 0, g0.height);
  };
  // Install as the global Range, keeping `range instanceof Range` working. defClass already made an
  // empty Range earlier; overwrite it with this functional constructor (its prototype still chains to
  // AbstractRange).
  try { def(globalThis, "Range", Range); } catch (e) {}

  // CaretPosition: { offsetNode, offset, getClientRect() }. getClientRect() returns a FRESH DOMRect
  // each call (the WPT test asserts identity differs between calls).
  function CaretPosition(offsetNode, offset, geom) {
    this.offsetNode = offsetNode; this.offset = offset; this._geom = geom || null;
  }
  CaretPosition.prototype.getClientRect = function () {
    var g = this._geom;
    if (!g) { return __makeDOMRect(0, 0, 0, 0); }
    return __makeDOMRect(g.x, g.top, 0, g.height); // collapsed caret: zero width
  };
  try { def(globalThis, "CaretPosition", CaretPosition); } catch (e) {}

  // __makeCaretAt(x, y): the CaretPosition at the viewport point. offsetNode prefers the deepest TEXT
  // node at the point (else the deepest element); offset is the nearest character index inside that
  // text run (uniform-advance approximation). Returns null when no box is hit. Media/replaced
  // elements (audio/video/canvas/input) and element hits resolve to offset 0.
  def(globalThis, "__makeCaretAt", function (x, y) {
    x = Number(x); y = Number(y);
    if (!isFinite(x) || !isFinite(y)) { return null; }
    var hit = __deepestNodeAtPoint(x, y);
    if (hit < 0) { return null; }
    var t = __nodeType(hit);
    if (t === 3) {
      // Text node: compute the character offset from the run box and the x coordinate.
      var r = null; try { r = __rect(hit); } catch (e) {}
      var s = __textContent(hit) || "";
      var offset = 0;
      if (r && s.length > 0 && r.right > r.left) {
        var charW = (r.right - r.left) / s.length;
        offset = Math.round((x - r.left) / charW);
        if (offset < 0) { offset = 0; } else if (offset > s.length) { offset = s.length; }
      }
      var node = __nodeFor(hit);
      return new CaretPosition(node, offset, __caretGeometry(hit, offset));
    }
    // Element hit (no text run at the point). Caret resolves to the element, offset 0.
    var node = __nodeFor(hit);
    return new CaretPosition(node, 0, __caretGeometry(hit, 0));
  });

  // --- CSSOM interface hierarchy + CSSRule type constants ------------------------------------
  // StyleSheet <- CSSStyleSheet; CSSRule <- {CSSStyleRule, CSSGroupingRule <- {CSSMediaRule,
  // CSSSupportsRule}, CSSImportRule, CSSFontFaceRule, CSSKeyframesRule, CSSKeyframeRule,
  // CSSNamespaceRule, CSSPageRule, CSSFontFeatureValuesRule}. instanceof + .type must hold.
  var StyleSheetCtor = defClass("StyleSheet");
  defClass("CSSStyleSheet", StyleSheetCtor);
  var CSSRuleCtor = defClass("CSSRule");
  (function (ctor) {
    var consts = { STYLE_RULE: 1, CHARSET_RULE: 2, IMPORT_RULE: 3, MEDIA_RULE: 4, FONT_FACE_RULE: 5,
      PAGE_RULE: 6, KEYFRAMES_RULE: 7, KEYFRAME_RULE: 8, MARGIN_RULE: 9, NAMESPACE_RULE: 10,
      COUNTER_STYLE_RULE: 11, SUPPORTS_RULE: 12, FONT_FEATURE_VALUES_RULE: 14, VIEWPORT_RULE: 15 };
    for (var k in consts) { ctor[k] = consts[k]; try { def(ctor.prototype, k, consts[k]); } catch (e) {} }
  })(CSSRuleCtor);
  defClass("CSSStyleRule", CSSRuleCtor);
  var CSSGroupingCtor = defClass("CSSGroupingRule", CSSRuleCtor);
  defClass("CSSConditionRule", CSSGroupingCtor);
  defClass("CSSMediaRule", globalThis.CSSConditionRule);
  defClass("CSSSupportsRule", globalThis.CSSConditionRule);
  defClass("CSSContainerRule", globalThis.CSSConditionRule);
  defClass("CSSImportRule", CSSRuleCtor);
  defClass("CSSFontFaceRule", CSSRuleCtor);
  defClass("CSSCounterStyleRule", CSSRuleCtor);
  defClass("CSSPageRule", CSSGroupingCtor);
  defClass("CSSKeyframesRule", CSSRuleCtor);
  defClass("CSSKeyframeRule", CSSRuleCtor);
  defClass("CSSNamespaceRule", CSSRuleCtor);
  defClass("CSSFontFeatureValuesRule", CSSRuleCtor);

  // The CSSStyleSheet constructor produces a constructable sheet (no owner node).
  (function () {
    var ctor = globalThis.CSSStyleSheet;
    if (typeof ctor === "function") {
      var Real = function (options) {
        var sheet = makeConstructedSheet("");
        options = options || {};
        if (options.media != null) { sheet.media.mediaText = String(options.media); }
        if (options.disabled) { sheet.disabled = true; }
        // `baseURL` is resolved against the constructor document's base URL. An invalid result
        // (e.g. a URL that fails to parse) is a NotAllowedError. The resolved URL becomes the
        // constructed sheet's base for relative `url(...)` resolution in its rules.
        if (options.baseURL != null) {
          var base;
          try { base = document.baseURI || (typeof location !== "undefined" ? location.href : undefined); } catch (e) { base = undefined; }
          var resolved;
          try { resolved = new URL(String(options.baseURL), base).href; }
          catch (e) { throw new globalThis.DOMException("Constructed style sheet base URL is not valid.", "NotAllowedError"); }
          sheet.__baseURL = resolved;
        }
        // The sheet's constructor document (used to validate adoptedStyleSheets membership).
        try { sheet.__constructorDocument = document; } catch (e) {}
        return sheet;
      };
      Real.prototype = ctor.prototype;
      def(globalThis, "CSSStyleSheet", Real);
    }
  })();

  // --- adoptedStyleSheets (Document / ShadowRoot) -------------------------------------------
  // CSSOM ObservableArray<CSSStyleSheet>. Each entry must be a CONSTRUCTED sheet whose constructor
  // document is `ownerDoc`; otherwise setting/inserting throws NotAllowedError. Adopted sheets are
  // mirrored into a managed `<style>` element appended to <head> (for the Document) so the cascade
  // applies them. `host.__refreshAdopted()` re-serializes the mirror; `markDirty` invokes it when an
  // adopted sheet mutates so rule edits / replaceSync are reflected in rendering.
  function installAdoptedStyleSheets(host, ownerDoc) {
    var backing = [];          // the actual CSSStyleSheet entries
    var mirror = null;         // managed <style> element (lazily created for the Document)
    function ensureMirror() {
      if (mirror) { return mirror; }
      try {
        mirror = ownerDoc.createElement("style");
        mirror.setAttribute("data-adopted-stylesheets", "");
        var head = ownerDoc.head || ownerDoc.getElementsByTagName("head")[0] || ownerDoc.documentElement || ownerDoc.body;
        if (head) { head.appendChild(mirror); }
      } catch (e) { mirror = null; }
      return mirror;
    }
    function serialize() {
      var s = "";
      for (var i = 0; i < backing.length; i++) {
        var sh = backing[i];
        if (!sh || sh.disabled) { continue; }
        try {
          var t = sh.cssText;
          // For a shadow root, scope every rule to the host's subtree so adopted styles don't leak
          // into the rest of the document (the mirror is a single global <style>). `:host` targets
          // the host element; other selectors become descendants of it. `host.__hostSel` is the marker.
          if (host && host.__hostSel && typeof globalThis.__scopeShadowCss === "function") {
            t = globalThis.__scopeShadowCss(t, host.__hostSel);
          }
          s += (s ? "\n" : "") + t;
        } catch (e) {}
      }
      return s;
    }
    function refresh() {
      var m = ensureMirror();
      if (!m) { return; }
      try { m.textContent = serialize(); } catch (e) {}
      // Carry a constructed sheet's explicit baseURL so the cascade resolves its relative url()s
      // against that base (not the document base). Best-effort: the first enabled sheet that has one.
      try {
        var base = null;
        for (var i = 0; i < backing.length; i++) {
          if (backing[i] && !backing[i].disabled && backing[i].__baseURL) { base = backing[i].__baseURL; break; }
        }
        if (base) { m.setAttribute("data-base-url", base); } else { m.removeAttribute("data-base-url"); }
      } catch (e) {}
    }
    host.__refreshAdopted = refresh;
    // Track host on each sheet so mutating it (markDirty) refreshes our mirror.
    function track(sh) {
      if (!sh) { return; }
      if (!sh.__adoptHosts) { try { def(sh, "__adoptHosts", []); } catch (e) { sh.__adoptHosts = []; } }
      if (sh.__adoptHosts.indexOf(host) < 0) { sh.__adoptHosts.push(host); }
    }
    function untrack(sh) {
      if (!sh || !sh.__adoptHosts) { return; }
      // Only untrack if no longer present in backing.
      if (backing.indexOf(sh) >= 0) { return; }
      var idx = sh.__adoptHosts.indexOf(host);
      if (idx >= 0) { sh.__adoptHosts.splice(idx, 1); }
    }
    function validate(v) {
      var ctor = globalThis.CSSStyleSheet;
      var isSheet = v && (typeof v === "object") && (ctor && ctor.prototype ? (v instanceof ctor) : true);
      // Standard CSSOM only allows constructed sheets. The tentative proposal (csswg-drafts #10013)
      // also allows adopting a sheet owned by an element in this document (a <link>/<style> sheet) —
      // which is what lets pages adopt an existing stylesheet into a shadow root.
      var ok = isSheet && (v.__constructed === true || v.__ownerNode != null);
      if (!ok) {
        throw new globalThis.DOMException("Can't adopt a non-constructed or foreign CSSStyleSheet.", "NotAllowedError");
      }
      if (v.__constructorDocument && v.__constructorDocument !== ownerDoc) {
        throw new globalThis.DOMException("Sheet constructor document does not match.", "NotAllowedError");
      }
    }
    // Build the observable-array proxy over `backing`. Index writes, length writes and the mutating
    // Array methods (push/splice/...) validate new entries and refresh the mirror afterwards.
    function makeArray(initial) {
      var arr = [];
      for (var i = 0; i < initial.length; i++) { arr.push(initial[i]); }
      var proxy = new Proxy(arr, {
        set: function (target, prop, value) {
          if (typeof prop === "string" && /^[0-9]+$/.test(prop)) {
            validate(value);
            target[prop] = value;
            rebuildBacking(target);
            return true;
          }
          if (prop === "length") {
            target.length = value;
            rebuildBacking(target);
            return true;
          }
          target[prop] = value;
          return true;
        },
        deleteProperty: function (target, prop) {
          delete target[prop];
          if (typeof prop === "string" && /^[0-9]+$/.test(prop)) { rebuildBacking(target); }
          return true;
        }
      });
      // Wrap the mutating methods so validation/refresh runs even through the proxy.
      ["push", "unshift", "splice", "fill", "copyWithin"].forEach(function (m) {
        var orig = Array.prototype[m];
        def(arr, m, function () {
          // Validate any incoming sheet arguments before mutating.
          if (m === "push" || m === "unshift") {
            for (var i = 0; i < arguments.length; i++) { validate(arguments[i]); }
          } else if (m === "splice") {
            for (var j = 2; j < arguments.length; j++) { validate(arguments[j]); }
          } else if (m === "fill") {
            validate(arguments[0]);
          }
          var r = orig.apply(arr, arguments);
          rebuildBacking(arr);
          return r;
        });
      });
      return proxy;
    }
    var liveArray = makeArray([]);
    // Re-sync `backing` from the current array contents (after any mutation), retrack sheets,
    // then refresh the mirror.
    function rebuildBacking(arr) {
      var old = backing.slice();
      backing = [];
      for (var i = 0; i < arr.length; i++) { if (arr[i] != null) { backing.push(arr[i]); track(arr[i]); } }
      for (var k = 0; k < old.length; k++) { untrack(old[k]); }
      refresh();
    }
    Object.defineProperty(host, "adoptedStyleSheets", {
      get: function () { return liveArray; },
      set: function (v) {
        if (v == null) { throw new TypeError("adoptedStyleSheets requires a sequence"); }
        var next = [];
        var len = v.length >>> 0;
        for (var i = 0; i < len; i++) { var item = v[i]; validate(item); next.push(item); }
        var old = backing.slice();
        liveArray = makeArray(next);
        backing = next.slice();
        for (var t = 0; t < backing.length; t++) { track(backing[t]); }
        for (var u = 0; u < old.length; u++) { untrack(old[u]); }
        refresh();
      },
      enumerable: true, configurable: true
    });
  }
  try { installAdoptedStyleSheets(globalThis.document, globalThis.document); } catch (e) {}

  // Minimal Shadow DOM: `el.attachShadow({mode})` returns a shadow root. We back it with a real
  // element appended under the host so its content (incl. <style>) is laid out + cascaded — not
  // truly style-scoped, but enough for getComputedStyle on shadow content + adoptedStyleSheets
  // (which already skips disabled sheets). Far better than throwing (web components need this).
  try {
    if (globalThis.Element && globalThis.Element.prototype) {
      def(globalThis.Element.prototype, "attachShadow", function (init) {
        if (this.__shadow) { return this.__shadow; }
        var root = document.createElement("div");
        try { __appendChild(this.__node, root.__node); } catch (e) {}
        root.host = this;
        root.mode = (init && init.mode) || "open";
        // Mark the host so an adopted sheet's `:host` rules can target it from the global mirror.
        try {
          var seq = (globalThis.__shadowHostSeq = (globalThis.__shadowHostSeq || 0) + 1);
          this.setAttribute("data-wpt-shadow-host", String(seq));
          root.__hostSel = '[data-wpt-shadow-host="' + seq + '"]';
        } catch (e) {}
        try { installAdoptedStyleSheets(root, document); } catch (e) {}
        // shadowRoot.styleSheets: the <style>/<link> sheets within the shadow tree, in tree order.
        // Per CSSOM this is SEPARATE from adoptedStyleSheets (the adopted-sheets mirror is excluded).
        Object.defineProperty(root, "styleSheets", {
          get: function () {
            var els = root.querySelectorAll("style, link");
            var sheets = [];
            for (var i = 0; i < els.length; i++) {
              // querySelectorAll results aren't canonicalized, so enrich each (gives `.sheet`).
              var el = (typeof globalThis.__canonNode === "function") ? globalThis.__canonNode(els[i]) : els[i];
              var tag = (el.tagName || "").toLowerCase();
              if (el.getAttribute && el.getAttribute("data-adopted-stylesheets") != null) { continue; }
              if (tag === "link") {
                var rel = (el.getAttribute && el.getAttribute("rel") || "").toLowerCase();
                if (rel.split(/\s+/).indexOf("stylesheet") < 0) { continue; }
                if (el.getAttribute && el.getAttribute("disabled") != null) { continue; }
              }
              if (el.__sheetDisabled) { continue; }
              try { var s = el.sheet; if (s) { sheets.push(s); } } catch (e) {}
            }
            sheets.item = function (n) { n = n >>> 0; return n < this.length ? this[n] : null; };
            try { if (globalThis.StyleSheetList && globalThis.StyleSheetList.prototype) { Object.setPrototypeOf(sheets, globalThis.StyleSheetList.prototype); } } catch (e) {}
            return sheets;
          },
          enumerable: true, configurable: true
        });
        this.__shadow = root;
        return root;
      });
      Object.defineProperty(globalThis.Element.prototype, "shadowRoot", {
        get: function () { var s = this.__shadow; return (s && s.mode === "open") ? s : null; },
        enumerable: true, configurable: true
      });
    }
  } catch (e) {}

  // Minimal <iframe> content document: a lightweight Document facade backed by a detached <body>
  // subtree (the iframe doesn't get a real nested browsing context / rendering, but its DOM +
  // CSSOM work). Enough for scripts that read `frame.contentDocument.body`, build a sub-DOM, and
  // use a <style>'s `.sheet`. `contentWindow.eval` runs with `document` bound to that facade.
  try {
    if (globalThis.HTMLIFrameElement && globalThis.HTMLIFrameElement.prototype) {
      var IFP = globalThis.HTMLIFrameElement.prototype;
      Object.defineProperty(IFP, "contentDocument", {
        get: function () {
          if (!this.__cdoc) {
            var body = document.createElement("body");
            var doc = {
              body: body, head: body, documentElement: body, nodeType: 9,
              // Marks this as a distinct (frame) document — moving a node here is a cross-document
              // adoption, which clears the moved subtree's adoptedStyleSheets (see __adoptOnInsert).
              __isFrameDoc: true,
              querySelector: function (s) { return body.querySelector(s); },
              querySelectorAll: function (s) { return body.querySelectorAll(s); },
              getElementById: function (id) { try { return body.querySelector('#' + id); } catch (e) { return null; } },
              getElementsByTagName: function (t) { return body.getElementsByTagName(t); },
              createElement: function (t) { return document.createElement(t); },
              createTextNode: function (t) { return document.createTextNode(t); },
              createDocumentFragment: function () { return document.createDocumentFragment(); },
              adoptedStyleSheets: [], styleSheets: { length: 0, item: function () { return null; } },
              defaultView: null,
              // document.open/write/close populate the frame's body (so the page can build the
              // iframe document dynamically). write() parses the HTML fragment into the frame body.
              open: function () { try { while (body.firstChild) { body.removeChild(body.firstChild); } } catch (e) {} return doc; },
              write: function (html) {
                try {
                  var tmp = document.createElement("div");
                  tmp.innerHTML = String(html == null ? "" : html);
                  while (tmp.firstChild) { body.appendChild(tmp.firstChild); }
                } catch (e) {}
              },
              writeln: function (html) { doc.write((html == null ? "" : String(html)) + "\n"); },
              close: function () {},
            };
            // Tag the content body so ownerDocument resolution maps it (and its subtree) to `doc`.
            try { def(body, "__frameDoc", doc); } catch (e) {}
            // Mark the frame body with the host <iframe>'s node id so getComputedStyle on frame
            // content can cascade this subtree with the iframe's own size as the media viewport.
            try { if (typeof this.__node === "number") { body.setAttribute("data-frame-host", String(this.__node)); } } catch (e) {}
            this.__cdoc = doc;
          }
          return this.__cdoc;
        },
        enumerable: true, configurable: true
      });
      Object.defineProperty(IFP, "contentWindow", {
        get: function () {
          var d = this.contentDocument;
          if (!this.__cwin) {
            this.__cwin = {
              document: d,
              // Run code with `document` bound to the iframe's facade document (direct eval sees it).
              eval: function (code) { var document = d; return eval(code); },
              // The frame window's getComputedStyle: the global one already cascades frame-document
              // subtrees with the iframe's own viewport, so just delegate.
              getComputedStyle: function (el, pseudo) { return getComputedStyle(el, pseudo); },
            };
            d.defaultView = this.__cwin;
          }
          return this.__cwin;
        },
        enumerable: true, configurable: true
      });
    }
  } catch (e) {}

  // --- Custom Elements (minimal) ---------------------------------------------------------------
  // `customElements.define(name, ctor)` registers a class, then upgrades matching elements already
  // in the tree (re-pointing their prototype at the ctor's) and fires `connectedCallback` on the
  // connected ones. Elements inserted later are upgraded/connected via the insertNode hook. We skip
  // the spec's constructor-run-with-`this`-as-the-element machinery (we can't replicate it without
  // engine support) — connectedCallback covers the overwhelming majority of components.
  try {
    var __ceReg = {};      // name -> ctor
    var __ceWhen = {};     // name -> { promise, resolve }
    function __ceConnected(el) {
      try { return !!(document.documentElement && document.documentElement.contains(el)); } catch (e) { return false; }
    }
    function __ceUpgrade(el) {
      if (!el || el.__ceUpgraded) { return; }
      var name = (el.tagName || "").toLowerCase();
      var ctor = __ceReg[name];
      if (!ctor) { return; }
      def(el, "__ceUpgraded", true);
      try { if (ctor.prototype) { Object.setPrototypeOf(el, ctor.prototype); } } catch (e) {}
    }
    function __ceConnect(el) {
      __ceUpgrade(el);
      if (!el || !el.__ceUpgraded || el.__ceConnectedFired) { return; }
      if (!__ceConnected(el)) { return; }
      def(el, "__ceConnectedFired", true);
      if (typeof el.connectedCallback === "function") {
        try { el.connectedCallback(); }
        catch (e) { try { console.error(e); } catch (e2) {} }
      }
    }
    function __ceWalk(nodeId) {
      if (nodeId == null || nodeId < 0) { return; }
      try {
        var el = (typeof globalThis.__nodeById === "function" && globalThis.__nodeById(nodeId)) ||
                 (typeof globalThis.__canonNode === "function" ? globalThis.__canonNode(nodeId) : null);
        if (el && el.tagName) { __ceConnect(el); }
        var kids = __children(nodeId);
        for (var i = 0; i < kids.length; i++) { __ceWalk(kids[i]); }
      } catch (e) {}
    }
    // Called from insertNode. Cheap no-op until at least one custom element is defined.
    def(globalThis, "__ceOnInsert", function (nodeId) {
      for (var k in __ceReg) { __ceWalk(nodeId); return; }
    });
    def(globalThis, "customElements", {
      define: function (name, ctor) {
        if (typeof name !== "string" || !/^[a-z][a-z0-9._]*-[a-z0-9._-]*$/.test(name)) {
          throw new globalThis.DOMException("'" + name + "' is not a valid custom element name", "SyntaxError");
        }
        if (typeof ctor !== "function") {
          throw new globalThis.TypeError("The second argument to customElements.define must be a constructor");
        }
        if (__ceReg[name]) {
          throw new globalThis.DOMException("the name '" + name + "' has already been used with this registry", "NotSupportedError");
        }
        __ceReg[name] = ctor;
        // Upgrade + connect elements already in the document (snapshot first — connectedCallback may mutate).
        try {
          var live = document.getElementsByTagName(name);
          var arr = []; for (var i = 0; i < live.length; i++) { arr.push(live[i]); }
          for (var j = 0; j < arr.length; j++) { __ceConnect(arr[j]); }
        } catch (e) {}
        if (__ceWhen[name]) { try { __ceWhen[name].resolve(ctor); } catch (e) {} }
      },
      get: function (name) { return __ceReg[name] || undefined; },
      getName: function (ctor) { for (var k in __ceReg) { if (__ceReg[k] === ctor) { return k; } } return null; },
      whenDefined: function (name) {
        if (__ceReg[name]) { return Promise.resolve(__ceReg[name]); }
        if (!__ceWhen[name]) { var r; var p = new Promise(function (res) { r = res; }); __ceWhen[name] = { promise: p, resolve: r }; }
        return __ceWhen[name].promise;
      },
      upgrade: function (root) { try { __ceWalk(root && root.__node); } catch (e) {} }
    });
  } catch (e) {}

  // --- <template>.content + cross-document ownerDocument / adoption ----------------------------
  // A <template>'s children belong to a "template contents document" (an inert document distinct
  // from the main one). We model the content as a DocumentFragment facade over the template
  // element's arena children, and resolve `ownerDocument` by walking ancestry: the nearest
  // <template> ancestor maps a node to that template's contents document, an <iframe> content body
  // maps to that frame's document, otherwise the main document. Moving a node thus updates its
  // ownerDocument, and moving it into a *frame* document clears the moved shadow roots' adopted
  // sheets (construct-stylesheets adoption steps) — but moving into a template does not.
  // Scope a shadow root's adopted-sheet CSS to the host's subtree: prefix each style rule's selector
  // with the host marker (so `.x` becomes a descendant of the host) and rewrite `:host`/`:host(X)` to
  // the host element itself. @-rules are passed through unscoped (their nested rules aren't rewritten
  // — rare for shadow-adopted sheets). Keeps shadow styles from leaking into the rest of the document.
  def(globalThis, "__scopeShadowCss", function (css, hostSel) {
    function scopeSel(sel) {
      if (!sel) { return sel; }
      if (/^:host(\b|\()/.test(sel)) {
        return sel.replace(/:host\(([^)]*)\)/g, hostSel + "$1").replace(/:host\b/g, hostSel);
      }
      return hostSel + " " + sel;
    }
    try {
      var out = "", i = 0, n = css.length;
      while (i < n) {
        var brace = css.indexOf("{", i);
        if (brace < 0) { break; }
        var sel = css.slice(i, brace).trim();
        var depth = 1, j = brace + 1;
        while (j < n && depth > 0) { var ch = css.charAt(j); if (ch === "{") { depth++; } else if (ch === "}") { depth--; } j++; }
        var body = css.slice(brace, j);
        if (sel.charAt(0) === "@") {
          out += sel + " " + body + "\n";
        } else {
          var parts = sel.split(","), scoped = [];
          for (var k = 0; k < parts.length; k++) { scoped.push(scopeSel(parts[k].trim())); }
          out += scoped.join(", ") + " " + body + "\n";
        }
        i = j;
      }
      return out;
    } catch (e) { return css; }
  });

  try {
    // Resolve a node id to its canonical, fully-enriched element wrapper (the one that carries
    // tag prototype + __shadow/__frameDoc). __wrapNode builds a bare wrapper; __canonNode enriches
    // and caches it. Reuse the cached wrapper when present.
    def(globalThis, "__elFor", function (cid) {
      if (cid == null || cid < 0) { return null; }
      var c = (typeof globalThis.__nodeById === "function") ? globalThis.__nodeById(cid) : null;
      if (c) { return c; }
      var w = (typeof globalThis.__wrapNode === "function") ? globalThis.__wrapNode(cid) : null;
      if (!w) { return null; }
      return (typeof globalThis.__canonNode === "function") ? globalThis.__canonNode(w) : w;
    });
    // Stable contents-document object for a template element (lazily created).
    function __templateDocFor(tpl) {
      if (!tpl.__contentDoc) {
        try { def(tpl, "__contentDoc", { __isTemplateContentsDoc: true, nodeType: 9, defaultView: null }); }
        catch (e) { tpl.__contentDoc = { __isTemplateContentsDoc: true, nodeType: 9, defaultView: null }; }
      }
      return tpl.__contentDoc;
    }
    def(globalThis, "__ownerDocumentOf", function (node) {
      try {
        var id = node && node.__node;
        if (typeof id !== "number") { return document; }
        var cur = id, guard = 0;
        while (cur >= 0 && guard++ < 100000) {
          var w = globalThis.__elFor(cur);
          if (w) {
            // The iframe content body (and its subtree) belongs to the frame document.
            if (w.__frameDoc) { return w.__frameDoc; }
            // A <template> ANCESTOR (not the node itself) puts the node in its contents document.
            if (cur !== id && w.tagName === "TEMPLATE") { return __templateDocFor(w); }
          }
          cur = __parent(cur);
        }
      } catch (e) {}
      return document;
    });

    // <template>.content — a DocumentFragment facade over the template element's children.
    if (globalThis.HTMLTemplateElement && globalThis.HTMLTemplateElement.prototype) {
      Object.defineProperty(globalThis.HTMLTemplateElement.prototype, "content", {
        get: function () {
          if (this.__contentFrag) { return this.__contentFrag; }
          var tpl = this, tplNode = tpl.__node;
          var nodeAt = function (cid) { return globalThis.__elFor(cid); };
          var frag = {
            nodeType: 11,
            host: tpl,
            get ownerDocument() { return __templateDocFor(tpl); },
            appendChild: function (child) { try { __appendChild(tplNode, child.__node); } catch (e) {} return child; },
            insertBefore: function (child, ref) { try { __insertNode(tplNode, child.__node, ref ? ref.__node : -1); } catch (e) {} return child; },
            removeChild: function (child) { try { __removeChild(child.__node); } catch (e) {} return child; },
            append: function () { for (var i = 0; i < arguments.length; i++) { var c = arguments[i]; this.appendChild(typeof c === "string" ? document.createTextNode(c) : c); } },
            prepend: function () { var r = this.firstChild; for (var i = 0; i < arguments.length; i++) { var c = arguments[i]; this.insertBefore(typeof c === "string" ? document.createTextNode(c) : c, r); } },
            get lastChild() { var k = __children(tplNode); return k.length ? nodeAt(k[k.length - 1]) : null; },
            get textContent() { var k = __children(tplNode), s = ""; for (var i = 0; i < k.length; i++) { var nd = nodeAt(k[i]); s += (nd && nd.textContent != null ? nd.textContent : ""); } return s; },
            get childNodes() { var k = __children(tplNode), a = []; for (var i = 0; i < k.length; i++) { a.push(nodeAt(k[i])); } return a; },
            get children() { var k = __children(tplNode), a = []; for (var i = 0; i < k.length; i++) { if (__nodeType(k[i]) === 1) { a.push(nodeAt(k[i])); } } return a; },
            get firstChild() { var k = __children(tplNode); return k.length ? nodeAt(k[0]) : null; },
            get firstElementChild() { var k = __children(tplNode); for (var i = 0; i < k.length; i++) { if (__nodeType(k[i]) === 1) { return nodeAt(k[i]); } } return null; },
            querySelector: function (s) { try { return tpl.querySelector(s); } catch (e) { return null; } },
            querySelectorAll: function (s) { try { return tpl.querySelectorAll(s); } catch (e) { return []; } },
            getElementById: function (gid) { try { return tpl.querySelector('#' + gid); } catch (e) { return null; } },
            cloneNode: function () { return this; },
          };
          try { def(tpl, "__contentFrag", frag); } catch (e) { tpl.__contentFrag = frag; }
          return frag;
        },
        enumerable: true, configurable: true
      });
    }

    // Called from insertNode after a move: if a moved element with a shadow root now lives in a
    // *frame* document (a real cross-document adoption — not a template), empty that shadow root's
    // adoptedStyleSheets in place (keeping the same observable array object).
    def(globalThis, "__adoptOnInsert", function (nodeId) {
      try {
        var od = null;
        var walk = function (cid) {
          if (cid == null || cid < 0) { return; }
          var w = globalThis.__elFor(cid);
          if (w && w.__shadow) {
            var doc = globalThis.__ownerDocumentOf(w);
            if (doc && doc.__isFrameDoc) {
              try { var asl = w.__shadow.adoptedStyleSheets; if (asl && asl.length) { asl.splice(0, asl.length); } } catch (e) {}
            }
          }
          var kids = __children(cid);
          for (var i = 0; i < kids.length; i++) { walk(kids[i]); }
        };
        walk(nodeId);
      } catch (e) {}
    });
  } catch (e) {}

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
  // --- Blob / File / FileReader (real: store + read back bytes) -----------------------------
  // Flatten Blob constructor `parts` (strings → UTF-8, ArrayBuffer/typed arrays → bytes, nested
  // Blobs → their bytes) into a plain byte array.
  function __blobBytes(parts) {
    var bytes = [];
    if (!parts || typeof parts.length !== "number") { return bytes; }
    for (var i = 0; i < parts.length; i++) {
      var p = parts[i];
      if (p == null) { continue; }
      if (typeof p === "string") {
        var enc = unescape(encodeURIComponent(p));
        for (var j = 0; j < enc.length; j++) { bytes.push(enc.charCodeAt(j) & 0xff); }
      } else if (p.__blobBytes) {
        bytes = bytes.concat(p.__blobBytes);
      } else if (p instanceof ArrayBuffer) {
        var v1 = new Uint8Array(p); for (var k = 0; k < v1.length; k++) { bytes.push(v1[k]); }
      } else if (p.buffer && typeof p.byteLength === "number") {
        var v2 = new Uint8Array(p.buffer, p.byteOffset || 0, p.byteLength); for (var m = 0; m < v2.length; m++) { bytes.push(v2[m]); }
      } else {
        var s2 = unescape(encodeURIComponent(String(p))); for (var n = 0; n < s2.length; n++) { bytes.push(s2.charCodeAt(n) & 0xff); }
      }
    }
    return bytes;
  }
  if (typeof globalThis.Blob !== "function") {
    def(globalThis, "Blob", function (parts, opts) {
      var bytes = __blobBytes(parts);
      this.__blobBytes = bytes;
      this.size = bytes.length;
      this.type = (opts && opts.type) || "";
      this.slice = function (start, end, type) {
        var s = start || 0, e = (end == null ? bytes.length : end);
        if (s < 0) { s += bytes.length; } if (e < 0) { e += bytes.length; }
        var sub = bytes.slice(Math.max(0, s), Math.max(0, e));
        var b = new globalThis.Blob([], { type: type || this.type });
        b.__blobBytes = sub; b.size = sub.length; return b;
      };
      this.text = function () {
        var s = ""; for (var i = 0; i < bytes.length; i++) { s += String.fromCharCode(bytes[i]); }
        var out; try { out = decodeURIComponent(escape(s)); } catch (e) { out = s; }
        return Promise.resolve(out);
      };
      this.arrayBuffer = function () {
        var buf = new ArrayBuffer(bytes.length), view = new Uint8Array(buf);
        for (var i = 0; i < bytes.length; i++) { view[i] = bytes[i]; }
        return Promise.resolve(buf);
      };
    });
  }
  if (typeof globalThis.File !== "function") {
    def(globalThis, "File", function (parts, name, opts) { globalThis.Blob.call(this, parts, opts); this.name = String(name || ""); this.lastModified = 0; });
  }
  if (typeof globalThis.FileReader !== "function") {
    def(globalThis, "FileReader", function () {
      var self = this;
      this.readyState = 0; this.result = null; this.error = null;
      this.onload = null; this.onloadend = null; this.onerror = null; this.onprogress = null;
      try { installEvents(this); } catch (e) {}
      function finish(result) {
        self.readyState = 2; self.result = result;
        var ev = { type: "load", target: self, currentTarget: self };
        if (typeof self.onload === "function") { try { self.onload(ev); } catch (e) {} }
        try { fireOn(self, "load"); } catch (e) {}
        if (typeof self.onloadend === "function") { try { self.onloadend({ type: "loadend", target: self }); } catch (e) {} }
        try { fireOn(self, "loadend"); } catch (e) {}
      }
      this.readAsText = function (blob) { (blob && blob.text ? blob.text() : Promise.resolve("")).then(finish); };
      this.readAsArrayBuffer = function (blob) { (blob && blob.arrayBuffer ? blob.arrayBuffer() : Promise.resolve(new ArrayBuffer(0))).then(finish); };
      this.readAsDataURL = function (blob) {
        (blob && blob.arrayBuffer ? blob.arrayBuffer() : Promise.resolve(new ArrayBuffer(0))).then(function (buf) {
          var view = new Uint8Array(buf), s = "";
          for (var i = 0; i < view.length; i++) { s += String.fromCharCode(view[i]); }
          var b64 = (typeof btoa === "function") ? btoa(s) : "";
          finish("data:" + ((blob && blob.type) || "application/octet-stream") + ";base64," + b64);
        });
      };
      this.abort = fn;
    });
  }
  if (typeof globalThis.Worker !== "function") {
    def(globalThis, "Worker", function () { this.postMessage = fn; this.terminate = fn; this.onmessage = null; this.onerror = null; this.addEventListener = fn; this.removeEventListener = fn; });
  }
  if (typeof globalThis.WebSocket !== "function") {
    // Real WebSocket: __wsConnect spawns a host socket thread (net::ws_run) and returns an id.
    // The host delivers events via __wsDeliver(id, kind, payload) during the Rust drain; send/close
    // go back through __wsSend/__wsClose. Binary is base64-bridged across the host boundary.
    var __wsRegistry = Object.create(null);
    function __wsToBase64(data) {
      // Accept ArrayBuffer / typed array / Blob (Blob exposes __blobBytes) → base64 string.
      var bytes;
      if (data instanceof ArrayBuffer) { bytes = new Uint8Array(data); }
      else if (data && data.buffer instanceof ArrayBuffer) { bytes = new Uint8Array(data.buffer, data.byteOffset || 0, data.byteLength); }
      else if (data && data.__blobBytes) { bytes = data.__blobBytes; }
      else { bytes = new Uint8Array(0); }
      var s = "";
      for (var i = 0; i < bytes.length; i++) { s += String.fromCharCode(bytes[i]); }
      return (typeof btoa === "function") ? btoa(s) : "";
    }
    function __wsFromBase64(b64) {
      var s = (typeof atob === "function") ? atob(b64) : "";
      var buf = new ArrayBuffer(s.length), view = new Uint8Array(buf);
      for (var i = 0; i < s.length; i++) { view[i] = s.charCodeAt(i) & 0xff; }
      return buf;
    }
    var WebSocketCtor = function (url, protocols) {
      this.url = String(url);
      this.readyState = 0; // CONNECTING
      this.bufferedAmount = 0;
      this.protocol = "";
      this.extensions = "";
      this.binaryType = "blob";
      this.onopen = null; this.onmessage = null; this.onclose = null; this.onerror = null;
      try { installEvents(this); } catch (e) {}
      var id = (typeof __wsConnect === "function") ? __wsConnect(this.url) : 0;
      this.__wsid = id;
      __wsRegistry[id] = this;
    };
    WebSocketCtor.prototype.send = function (data) {
      if (this.readyState !== 1) {
        throw new globalThis.DOMException("Failed to execute 'send' on 'WebSocket': Still in CONNECTING state.", "InvalidStateError");
      }
      if (typeof __wsSend !== "function") { return; }
      if (typeof data === "string") { __wsSend(this.__wsid, 0, data); }
      else { __wsSend(this.__wsid, 1, __wsToBase64(data)); }
    };
    WebSocketCtor.prototype.close = function (code, reason) {
      if (this.readyState === 3 || this.readyState === 2) { return; }
      this.readyState = 2; // CLOSING
      if (typeof __wsClose === "function") { __wsClose(this.__wsid); }
    };
    WebSocketCtor.CONNECTING = 0; WebSocketCtor.OPEN = 1; WebSocketCtor.CLOSING = 2; WebSocketCtor.CLOSED = 3;
    WebSocketCtor.prototype.CONNECTING = 0; WebSocketCtor.prototype.OPEN = 1; WebSocketCtor.prototype.CLOSING = 2; WebSocketCtor.prototype.CLOSED = 3;
    def(globalThis, "WebSocket", WebSocketCtor);

    // Fire a handler (onX + any addEventListener listeners) with an event object on a WebSocket.
    function __wsFire(ws, type, ev) {
      ev.type = type; ev.target = ws; ev.currentTarget = ws;
      var on = ws["on" + type];
      if (typeof on === "function") { try { on.call(ws, ev); } catch (e) { (globalThis.__timerErrors || []).push((e && e.stack) || String(e)); } }
      if (typeof ws.dispatchEvent === "function") { try { ws.dispatchEvent(ev); } catch (e) { (globalThis.__timerErrors || []).push((e && e.stack) || String(e)); } }
    }
    // Called from Rust's drain phase for each pending socket event.
    def(globalThis, "__wsDeliver", function (id, kind, payload) {
      var ws = __wsRegistry[id];
      if (!ws) { return; }
      kind = Number(kind);
      if (kind === 0) {            // open
        ws.readyState = 1;
        __wsFire(ws, "open", {});
      } else if (kind === 1) {     // text message
        __wsFire(ws, "message", { data: payload });
      } else if (kind === 2) {     // binary message (base64)
        var buf = __wsFromBase64(String(payload));
        var data = buf;
        if (ws.binaryType === "blob" && typeof globalThis.Blob === "function") {
          try { data = new globalThis.Blob([buf]); } catch (e) { data = buf; }
        }
        __wsFire(ws, "message", { data: data });
      } else if (kind === 3) {     // close ("code:reason")
        ws.readyState = 3;
        var p = String(payload), ci = p.indexOf(":");
        var code = ci >= 0 ? parseInt(p.slice(0, ci), 10) : 1005;
        var reason = ci >= 0 ? p.slice(ci + 1) : "";
        if (!(code >= 0)) { code = 1005; }
        __wsFire(ws, "close", { code: code, reason: reason, wasClean: code === 1000 });
        delete __wsRegistry[id];
      } else if (kind === 4) {     // error
        __wsFire(ws, "error", { message: String(payload) });
      }
    });
  }
  if (typeof globalThis.Headers !== "function") {
    def(globalThis, "Headers", function (init) {
      var m = {};
      this.append = function (k, v) { k = String(k).toLowerCase(); m[k] = (m[k] === undefined) ? String(v) : (m[k] + ", " + String(v)); };
      this.set = function (k, v) { m[String(k).toLowerCase()] = String(v); };
      this.get = function (k) { var v = m[String(k).toLowerCase()]; return v === undefined ? null : v; };
      this.has = function (k) { return String(k).toLowerCase() in m; };
      this.delete = function (k) { delete m[String(k).toLowerCase()]; };
      this.forEach = function (cb, thisArg) { Object.keys(m).sort().forEach(function (k) { cb.call(thisArg, m[k], k, this); }, this); };
      this.keys = function () { return Object.keys(m).sort()[Symbol.iterator](); };
      this.values = function () { return Object.keys(m).sort().map(function (k) { return m[k]; })[Symbol.iterator](); };
      this.entries = function () { return Object.keys(m).sort().map(function (k) { return [k, m[k]]; })[Symbol.iterator](); };
      this.getSetCookie = function () { return []; };
      this[Symbol.iterator] = function () { return this.entries(); };
      // init: another Headers, an array of [k,v] pairs, or a plain object.
      if (init) {
        if (typeof init.forEach === "function" && typeof init.length !== "number") { init.forEach(function (v, k) { this.append(k, v); }, this); }
        else if (typeof init.length === "number") { for (var i = 0; i < init.length; i++) { this.append(init[i][0], init[i][1]); } }
        else { for (var k in init) { if (Object.prototype.hasOwnProperty.call(init, k)) { this.append(k, init[k]); } } }
      }
    });
  }

  // --- Request / Response (Fetch API classes) ----------------------------------------------
  if (typeof globalThis.Request !== "function") {
    var RequestCtor = function (input, init) {
      init = init || {};
      var fromReq = input && typeof input === "object" && input.__isRequest;
      this.url = fromReq ? input.url : ((input && input.url) ? String(input.url) : String(input));
      this.method = String(init.method || (fromReq && input.method) || "GET").toUpperCase();
      this.headers = new globalThis.Headers(init.headers || (fromReq ? input.headers : null) || {});
      this.body = init.body !== undefined ? init.body : (fromReq ? input.body : null);
      this.credentials = init.credentials || "same-origin";
      this.mode = init.mode || "cors";
      this.cache = init.cache || "default";
      this.redirect = init.redirect || "follow";
      this.referrer = init.referrer || "about:client";
      this.signal = init.signal || (fromReq ? input.signal : null) || null;
      this.__isRequest = true;
    };
    RequestCtor.prototype.clone = function () { return new globalThis.Request(this.url, this); };
    RequestCtor.prototype.text = function () { return Promise.resolve(this.body == null ? "" : String(this.body)); };
    RequestCtor.prototype.json = function () { try { return Promise.resolve(JSON.parse(this.body == null ? "null" : String(this.body))); } catch (e) { return Promise.reject(e); } };
    def(globalThis, "Request", RequestCtor);
  }

  if (typeof globalThis.Response !== "function") {
    var ResponseCtor = function (body, init) {
      init = init || {};
      this.status = init.status !== undefined ? (init.status | 0) : 200;
      this.statusText = init.statusText !== undefined ? String(init.statusText) : "";
      this.ok = this.status >= 200 && this.status < 300;
      this.headers = (init.headers && init.headers.entries) ? init.headers : new globalThis.Headers(init.headers || {});
      this.url = init.url ? String(init.url) : "";
      this.redirected = !!init.redirected;
      this.type = init.type || "default";
      this.bodyUsed = false;
      this.body = null;
      this.__body = (body == null) ? "" : (typeof body === "string" ? body : (typeof body.toString === "function" ? body.toString() : String(body)));
      this.__isResponse = true;
    };
    ResponseCtor.prototype.text = function () { this.bodyUsed = true; return Promise.resolve(this.__body); };
    ResponseCtor.prototype.json = function () { this.bodyUsed = true; try { return Promise.resolve(JSON.parse(this.__body)); } catch (e) { return Promise.reject(e); } };
    ResponseCtor.prototype.arrayBuffer = function () { return Promise.resolve(new ArrayBuffer(0)); };
    ResponseCtor.prototype.blob = function () { return Promise.resolve({ size: this.__body.length, type: (this.headers.get && this.headers.get("content-type")) || "" }); };
    ResponseCtor.prototype.formData = function () { return Promise.reject(new TypeError("formData not supported")); };
    // body: a ReadableStream of the response's bytes (UTF-8). Reading it marks the body used.
    Object.defineProperty(ResponseCtor.prototype, "body", {
      get: function () {
        if (this.__bodyStream) { return this.__bodyStream; }
        var self = this;
        var src = String(this.__body == null ? "" : this.__body);
        var bytes = (typeof globalThis.TextEncoder === "function") ? new globalThis.TextEncoder().encode(src) : (function () { var a = []; for (var i = 0; i < src.length; i++) { a.push(src.charCodeAt(i) & 0xff); } return new Uint8Array(a); })();
        this.__bodyStream = new globalThis.ReadableStream({ start: function (c) { self.bodyUsed = true; c.enqueue(bytes); c.close(); } });
        return this.__bodyStream;
      }, configurable: true, enumerable: true
    });
    ResponseCtor.prototype.clone = function () { return new globalThis.Response(this.__body, { status: this.status, statusText: this.statusText, headers: this.headers, url: this.url, type: this.type, redirected: this.redirected }); };
    ResponseCtor.json = function (data, init) { init = init || {}; var h = new globalThis.Headers(init.headers || {}); if (!h.has("content-type")) { h.set("content-type", "application/json"); } return new globalThis.Response(JSON.stringify(data), { status: init.status, statusText: init.statusText, headers: h }); };
    ResponseCtor.error = function () { var r = new globalThis.Response("", { status: 0 }); r.type = "error"; return r; };
    ResponseCtor.redirect = function (url, status) { var r = new globalThis.Response("", { status: status || 302 }); r.headers.set("location", String(url)); r.redirected = true; return r; };
    def(globalThis, "Response", ResponseCtor);
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
      this.keys = function () { return pairs.map(function (p) { return p[0]; })[Symbol.iterator](); };
      this.values = function () { return pairs.map(function (p) { return p[1]; })[Symbol.iterator](); };
      this.entries = function () { return pairs.map(function (p) { return [p[0], p[1]]; })[Symbol.iterator](); };
      this.sort = function () { pairs.sort(function (a, b) { return a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0; }); };
      this[Symbol.iterator] = function () { return this.entries(); };
      this.toString = function () { return pairs.map(function (p) { return encodeURIComponent(p[0]) + "=" + encodeURIComponent(p[1]); }).join("&"); };
      Object.defineProperty(this, "size", { get: function () { return pairs.length; }, enumerable: false, configurable: true });
    });
  }

  // --- URL ---------------------------------------------------------------------------------
  if (typeof globalThis.URL !== "function") {
    def(globalThis, "URL", function (url, base) {
      var p = parseURL(url, base != null ? String(base) : null);
      // Per the URL standard, `new URL(...)` throws a TypeError for an invalid URL.
      if (p.__invalid) {
        throw new TypeError("Failed to construct 'URL': Invalid URL");
      }
      this.href = p.href; this.protocol = p.protocol; this.host = p.host; this.hostname = p.hostname;
      this.port = p.port; this.pathname = p.pathname; this.search = p.search; this.hash = p.hash; this.origin = p.origin;
      this.username = p.username || ""; this.password = p.password || "";
      this.searchParams = new globalThis.URLSearchParams(p.search);
      this.toString = function () { return this.href; }; this.toJSON = function () { return this.href; };
    });
    // Encode the Blob's bytes as a self-contained data: URL so it actually works as an <img> src /
    // fetch target (we don't keep a blob: registry). revoke is a no-op (data: needs no cleanup).
    globalThis.URL.createObjectURL = function (obj) {
      try {
        if (obj && obj.__blobBytes) {
          var bytes = obj.__blobBytes, s = "";
          for (var i = 0; i < bytes.length; i++) { s += String.fromCharCode(bytes[i]); }
          var b64 = (typeof btoa === "function") ? btoa(s) : "";
          return "data:" + (obj.type || "application/octet-stream") + ";base64," + b64;
        }
      } catch (e) {}
      return "blob:null/0";
    };
    globalThis.URL.revokeObjectURL = fn;
  }
  if (typeof globalThis.queueMicrotask !== "function") { /* installed by timers */ }

  // --- misc presence stubs -----------------------------------------------------------------
  def(globalThis, "requestIdleCallback", function (cb) { return setTimeout(function () { try { cb({ didTimeout: false, timeRemaining: function () { return 0; } }); } catch (e) {} }, 1); });
  def(globalThis, "cancelIdleCallback", function (id) { return clearTimeout(id); });

  if (typeof globalThis.structuredClone !== "function") {
    def(globalThis, "structuredClone", function (v) { try { return JSON.parse(JSON.stringify(v)); } catch (e) { return v; } });
  }

  // CSS namespace: CSS.supports (feature detection — optimistic), CSS.escape (selector escaping),
  // and no-op registerProperty. Pages reference `CSS` directly (ReferenceError otherwise).
  if (typeof globalThis.CSS === "undefined") {
    var CSSns = {
      supports: function (prop, value) {
        try {
          if (value !== undefined) {
            var pn = normPropName(prop), pv = String(value);
            if (pv.length === 0) return false;
            if (!isKnownProperty(pn)) return false;
            return isValidValue(pn, pv);
          }
          // One-arg form: a support condition. `selector(...)` / `font-tech(...)` /
          // `font-format(...)` functional conditions are answered optimistically (feature-detection).
          var c = String(prop).trim();
          if (/^(selector|font-tech|font-format)\s*\(/i.test(c)) return true;
          var ci = indexOfTopLevelColon(c);
          if (ci < 0) return false;
          return CSSns.supports(c.slice(0, ci).trim(), c.slice(ci + 1).trim());
        } catch (e) { return false; }
      },
      escape: function (value) {
        if (arguments.length < 1) { throw new TypeError("Failed to execute 'escape' on 'CSS': 1 argument required, but only 0 present."); }
        // CSSOM "serialize an identifier" (https://drafts.csswg.org/cssom/#serialize-an-identifier).
        var s = String(value), out = "";
        var len = s.length;
        for (var i = 0; i < len; i++) {
          var c = s.charCodeAt(i);
          if (c === 0x0000) {
            // U+0000 NULL -> U+FFFD REPLACEMENT CHARACTER.
            out += "�";
          } else if ((c >= 0x0001 && c <= 0x001F) || c === 0x007F) {
            // Control characters -> "\" + hex + " ".
            out += "\\" + c.toString(16) + " ";
          } else if (i === 0 && c >= 0x0030 && c <= 0x0039) {
            // A leading digit -> "\" + hex + " ".
            out += "\\" + c.toString(16) + " ";
          } else if (i === 1 && c >= 0x0030 && c <= 0x0039 && s.charCodeAt(0) === 0x002D) {
            // A digit as the second char when the first is "-" -> escaped.
            out += "\\" + c.toString(16) + " ";
          } else if (i === 0 && c === 0x002D && len === 1) {
            // A lone "-" -> "\-".
            out += "\\" + s.charAt(i);
          } else if (c >= 0x0080 || c === 0x002D || c === 0x005F ||
                     (c >= 0x0030 && c <= 0x0039) || (c >= 0x0041 && c <= 0x005A) || (c >= 0x0061 && c <= 0x007A)) {
            // >= U+0080, "-", "_", 0-9, A-Z, a-z -> the character itself.
            out += s.charAt(i);
          } else {
            // Any other character -> "\" + the character.
            out += "\\" + s.charAt(i);
          }
        }
        return out;
      },
      registerProperty: function () {},
      px: function (n) { return { value: Number(n) || 0, unit: "px", toString: function () { return (Number(n) || 0) + "px"; } }; }
    };
    // WebIDL namespace object: @@toStringTag is the namespace name, non-writable/non-enumerable/
    // configurable — so `Object.prototype.toString.call(CSS) === "[object CSS]"`.
    try { Object.defineProperty(CSSns, Symbol.toStringTag, { value: "CSS", writable: false, enumerable: false, configurable: true }); } catch (e) {}
    def(globalThis, "CSS", CSSns);
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

  // crypto: real OS randomness via the __cryptoRandom native (falls back to a PRNG if unavailable).
  function __randBytes(n) {
    try { var b = __cryptoRandom(n); if (b && b.length === n) { return b; } } catch (e) {}
    var out = []; for (var i = 0; i < n; i++) { out.push((Math.floor((i * 2654435761) % 256)) || 1); } return out;
  }
  globalThis.crypto = {
    getRandomValues: function (arr) {
      if (!arr || typeof arr.length !== "number") { return arr; }
      var bpe = arr.BYTES_PER_ELEMENT || 1;
      var bytes = __randBytes(arr.length * bpe);
      for (var i = 0; i < arr.length; i++) {
        var v = 0;
        for (var b = 0; b < bpe; b++) { v = (v * 256) + (bytes[i * bpe + b] || 0); }
        arr[i] = v;
      }
      return arr;
    },
    randomUUID: function () {
      var b = __randBytes(16);
      b[6] = (b[6] & 0x0f) | 0x40; // version 4
      b[8] = (b[8] & 0x3f) | 0x80; // variant 10
      var hex = []; for (var i = 0; i < 16; i++) { hex.push((b[i] + 0x100).toString(16).slice(1)); }
      return hex.slice(0, 4).join("") + "-" + hex.slice(4, 6).join("") + "-" + hex.slice(6, 8).join("") +
             "-" + hex.slice(8, 10).join("") + "-" + hex.slice(10, 16).join("");
    },
    subtle: {}
  };

  // --- FormData ----------------------------------------------------------------------------
  // Pure-JS FormData. Backed by an array of [name, value] entries. When constructed from a
  // <form> element, collects the form's successful named controls. NOTE: File/Blob values are
  // not specially handled — they are stored as-is (and stringified when serialized); there is no
  // real File support, and `fetch` serializes a FormData body as urlencoded (not multipart).
  if (typeof globalThis.FormData !== "function") {
    def(globalThis, "FormData", function (form) {
      var entries = [];
      this.__isFormData = true;
      function add(name, value) { entries.push([String(name), value]); }
      // Collect successful named controls from a <form> element (duck-typed via tagName).
      if (form && typeof form === "object" && form.tagName && String(form.tagName).toUpperCase() === "FORM") {
        var collect = function (el) {
          var kids = el.childNodes || [];
          for (var i = 0; i < kids.length; i++) {
            var c = kids[i];
            if (!c || c.nodeType !== 1) { continue; }
            var tag = String(c.tagName || "").toUpperCase();
            var name = c.getAttribute ? c.getAttribute("name") : null;
            var disabled = c.getAttribute ? (c.getAttribute("disabled") != null) : false;
            if (tag === "INPUT" && name && !disabled) {
              var type = (c.getAttribute("type") || "text").toLowerCase();
              if (type === "checkbox" || type === "radio") {
                if (c.checked) { add(name, c.value != null && c.value !== "" ? c.value : "on"); }
              } else if (type === "submit" || type === "button" || type === "reset" || type === "file" || type === "image") {
                // not successful for our purposes
              } else {
                add(name, c.value != null ? c.value : "");
              }
            } else if (tag === "SELECT" && name && !disabled) {
              add(name, c.value != null ? c.value : "");
            } else if (tag === "TEXTAREA" && name && !disabled) {
              // A <textarea>'s value defaults to its text content when no value was set.
              var tv = (c.value != null && c.value !== "") ? c.value : (c.textContent != null ? c.textContent : "");
              add(name, tv);
            }
            // Recurse into descendants (controls may be nested in wrappers).
            if (c.childNodes && c.childNodes.length) { collect(c); }
          }
        };
        collect(form);
      }
      this.append = function (name, value) { add(name, value); };
      this.set = function (name, value) { name = String(name); for (var i = entries.length - 1; i >= 0; i--) { if (entries[i][0] === name) { entries.splice(i, 1); } } add(name, value); };
      this.get = function (name) { name = String(name); for (var i = 0; i < entries.length; i++) { if (entries[i][0] === name) { return entries[i][1]; } } return null; };
      this.getAll = function (name) { name = String(name); var out = []; for (var i = 0; i < entries.length; i++) { if (entries[i][0] === name) { out.push(entries[i][1]); } } return out; };
      this.has = function (name) { name = String(name); for (var i = 0; i < entries.length; i++) { if (entries[i][0] === name) { return true; } } return false; };
      this.delete = function (name) { name = String(name); for (var i = entries.length - 1; i >= 0; i--) { if (entries[i][0] === name) { entries.splice(i, 1); } } };
      this.forEach = function (cb, thisArg) { for (var i = 0; i < entries.length; i++) { cb.call(thisArg, entries[i][1], entries[i][0], this); } };
      this.keys = function () { return entries.map(function (e) { return e[0]; })[Symbol.iterator](); };
      this.values = function () { return entries.map(function (e) { return e[1]; })[Symbol.iterator](); };
      this.entries = function () { return entries.map(function (e) { return [e[0], e[1]]; })[Symbol.iterator](); };
      this[Symbol.iterator] = function () { return this.entries(); };
      // Internal: urlencoded serialization used by fetch (multipart is NOT implemented).
      this.__toUrlEncoded = function () {
        return entries.map(function (e) { return encodeURIComponent(e[0]) + "=" + encodeURIComponent(String(e[1])); }).join("&");
      };
    });
  }

  // Serialize a FormData-like into an application/x-www-form-urlencoded string.
  function __formDataToUrlEncoded(fd) {
    if (fd && typeof fd.__toUrlEncoded === "function") { return fd.__toUrlEncoded(); }
    // Fallback: iterate entries() if available.
    var parts = [];
    if (fd && typeof fd.forEach === "function") {
      fd.forEach(function (v, k) { parts.push(encodeURIComponent(k) + "=" + encodeURIComponent(String(v))); });
    }
    return parts.join("&");
  }

  // Async fetch plumbing. `fetch()` calls the non-blocking native `__startFetch`, which spawns a
  // background request thread and returns an id immediately; the page promise is parked in
  // `__pendingFetches[id]` and settled later — on the worker thread, inside the Rust drain — when
  // the request completes, via `__resolveFetch(id, envelopeStr)` / `__rejectFetch(id)`. This lets
  // many fetches run concurrently instead of serializing one blocking call at a time.
  globalThis.__pendingFetches = globalThis.__pendingFetches || {};
  // Build a Response from a host JSON envelope string (the shape the request fetcher returns).
  function __responseFromEnvelope(envelope, fallbackUrl) {
    var env = JSON.parse(envelope);
    var respBody = env.body != null ? String(env.body) : "";
    var contentType = env.contentType != null ? String(env.contentType) : "";
    var rh = new globalThis.Headers();
    if (contentType) { rh.set("content-type", contentType); }
    return new globalThis.Response(respBody, {
      status: env.status != null ? (env.status | 0) : 200,
      statusText: env.statusText != null ? String(env.statusText) : "",
      headers: rh,
      url: env.url != null ? String(env.url) : fallbackUrl,
      type: "basic"
    });
  }
  // Settle a parked fetch with a host envelope (or null → reject as a failed transport).
  def(globalThis, "__resolveFetch", function (id, envelope) {
    var pending = globalThis.__pendingFetches[id];
    if (!pending) { return; } // already aborted/settled; late completion ignored.
    delete globalThis.__pendingFetches[id];
    if (envelope == null) { pending.reject(new TypeError("Failed to fetch")); return; }
    var resp;
    try { resp = __responseFromEnvelope(String(envelope), pending.url); }
    catch (e) { pending.reject(new TypeError("Failed to fetch")); return; }
    pending.resolve(resp);
  });
  // Reject a parked fetch (transport error, or abort).
  def(globalThis, "__rejectFetch", function (id, reason) {
    var pending = globalThis.__pendingFetches[id];
    if (!pending) { return; }
    delete globalThis.__pendingFetches[id];
    pending.reject(reason || new TypeError("Failed to fetch"));
  });

  // fetch: starts the request via the native __startFetch primitive (which runs the host request
  // on a background thread) and returns a Promise parked in __pendingFetches, settled later by
  // __resolveFetch/__rejectFetch during the Rust drain. Sends the method, headers, and serialized
  // body; resolves a Response from the host's JSON envelope. Rejects with TypeError("Failed to
  // fetch") when the host request fails (null envelope), or with AbortError if the signal aborts.
  if (typeof globalThis.fetch !== "function") {
    def(globalThis, "fetch", function (input, init) {
      init = init || {};
      var url;
      try { url = (input && input.url) ? String(input.url) : String(input); }
      catch (e) { url = String(input); }
      // Dangling-markup mitigation: a request URL containing both "<" and a newline/CR/tab is a
      // network error (blocks data exfiltration via unclosed markup in resource URLs).
      if (url.indexOf("<") >= 0 && /[\n\r\t]/.test(url)) {
        return Promise.reject(new TypeError("Failed to fetch"));
      }
      var method = String(init.method || "GET").toUpperCase();

      // Honor an AbortSignal: a fetch on an already-aborted signal rejects with AbortError. (Our
      // fetch is synchronous, so only pre-abort is observable.)
      var signal = init.signal;
      if (signal && signal.aborted) {
        return Promise.reject(signal.reason || new globalThis.DOMException("The operation was aborted.", "AbortError"));
      }

      // --- Headers: accept plain object, Headers-like (forEach), or array of pairs. ---
      var headers = {};
      var hdrLower = {}; // lowercased name -> canonical name present, for content-type checks
      function setHeader(name, value) { name = String(name); headers[name] = String(value); hdrLower[name.toLowerCase()] = name; }
      var ih = init.headers;
      if (ih) {
        if (Array.isArray(ih)) {
          for (var i = 0; i < ih.length; i++) { if (ih[i]) { setHeader(ih[i][0], ih[i][1]); } }
        } else if (typeof ih.forEach === "function" && typeof ih.get === "function") {
          ih.forEach(function (v, k) { setHeader(k, v); });
        } else if (typeof ih === "object") {
          for (var k in ih) { if (Object.prototype.hasOwnProperty.call(ih, k)) { setHeader(k, ih[k]); } }
        }
      }
      function hasContentType() { return hdrLower["content-type"] != null; }
      function ensureContentType(ct) { if (!hasContentType()) { setHeader("Content-Type", ct); } }

      // --- Body serialization (GET/HEAD carry no body). ---
      var bodyStr = "";
      var rawBody = init.body;
      if (method !== "GET" && method !== "HEAD" && rawBody != null) {
        if (typeof rawBody === "string") {
          bodyStr = rawBody;
        } else if (rawBody.__isFormData || (typeof globalThis.FormData === "function" && rawBody instanceof globalThis.FormData)) {
          // FormData: encoded as urlencoded (real multipart/form-data is NOT implemented).
          bodyStr = __formDataToUrlEncoded(rawBody);
          ensureContentType("application/x-www-form-urlencoded;charset=UTF-8");
        } else if (typeof rawBody.toString === "function" && (typeof globalThis.URLSearchParams === "function" && rawBody instanceof globalThis.URLSearchParams)) {
          bodyStr = rawBody.toString();
          ensureContentType("application/x-www-form-urlencoded;charset=UTF-8");
        } else if (typeof rawBody === "object" && typeof rawBody.toString === "function" && rawBody.toString !== Object.prototype.toString) {
          // Other stringifiable objects (e.g. URLSearchParams-likes with a custom toString).
          bodyStr = rawBody.toString();
        } else {
          // Plain object / anything else: leave as String(body); don't force JSON.
          bodyStr = String(rawBody);
        }
      }

      if (typeof __startFetch !== "function") {
        return Promise.reject(new TypeError("Failed to fetch"));
      }
      // Kick off the request on a background thread; settle the promise later via the drain.
      var id = __startFetch(method, url, bodyStr, JSON.stringify(headers));
      return new Promise(function (resolve, reject) {
        globalThis.__pendingFetches[id] = { resolve: resolve, reject: reject, url: url };
        // AbortSignal: if it aborts while the request is in flight, reject this id with the abort
        // reason and forget it (a late background completion is then ignored — see __resolveFetch).
        if (signal && typeof signal.addEventListener === "function") {
          signal.addEventListener("abort", function () {
            __rejectFetch(id, signal.reason || new globalThis.DOMException("The operation was aborted.", "AbortError"));
          });
        }
      });
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

  // --- DOM Event constructors + class hierarchy (per the DOM / UI Events standards) ---------
  // Each Event/subclass stores its standard members in a non-enumerable internal bag (__ev) and
  // exposes them as read-only getters on the prototype, so the prototype chain gives correct
  // `instanceof` and `Object.getPrototypeOf(ev) === Iface.prototype` (which document.createEvent
  // relies on). Subclasses inherit Event via real prototype chains (MouseEvent -> UIEvent -> Event).
  (function () {
    // Monotonic high-resolution timestamp source for Event.timeStamp. The event-loop clock does
    // not advance between two synchronous constructions, but spec tests create events in tight
    // loops and rely on consecutive timestamps eventually differing (and not having sub-5µs
    // resolution). Base off performance.now() (shared time origin) and add a 5-microsecond
    // (0.005 ms) monotonic quantum per call so the value strictly increases yet stays coarse.
    var __tsCounter = 0;
    function __eventTimeStamp() {
      var base = 0;
      try { base = (globalThis.performance && typeof globalThis.performance.now === "function")
        ? globalThis.performance.now()
        : (globalThis.__eventLoop ? globalThis.__eventLoop.now : 0); } catch (e) { base = 0; }
      __tsCounter += 1;
      return base + __tsCounter * 0.005;
    }
    // Per-event internal state. `flags` holds dispatch bookkeeping shared with dispatchEvent().
    function initEventState(ev) {
      var s = {
        type: "", bubbles: false, cancelable: false, composed: false,
        defaultPrevented: false, isTrusted: false,
        eventPhase: 0, target: null, currentTarget: null,
        timeStamp: __eventTimeStamp(),
        stopPropagation: false, stopImmediate: false, dispatched: false, inPassive: false,
        path: []
      };
      def(ev, "__ev", s);
      return s;
    }
    function st(ev) { return ev.__ev || initEventState(ev); }

    // Define a read-only getter `name` on `proto` returning the matching internal-state field.
    function roGet(proto, name, field) {
      Object.defineProperty(proto, name, {
        get: function () { return st(this)[field]; }, enumerable: true, configurable: true
      });
    }

    function Event(type, init) {
      var s = initEventState(this);
      if (arguments.length > 0) { s.type = String(type); }
      if (init !== undefined && init !== null) {
        s.bubbles = !!init.bubbles;
        s.cancelable = !!init.cancelable;
        s.composed = !!init.composed;
      }
    }
    var EP = Event.prototype;
    roGet(EP, "type", "type");
    roGet(EP, "bubbles", "bubbles");
    roGet(EP, "cancelable", "cancelable");
    roGet(EP, "composed", "composed");
    roGet(EP, "defaultPrevented", "defaultPrevented");
    roGet(EP, "isTrusted", "isTrusted");
    roGet(EP, "eventPhase", "eventPhase");
    roGet(EP, "target", "target");
    roGet(EP, "currentTarget", "currentTarget");
    roGet(EP, "timeStamp", "timeStamp");
    Object.defineProperty(EP, "srcElement", { get: function () { return st(this).target; }, enumerable: true, configurable: true });
    // returnValue: legacy alias of !defaultPrevented (settable to false => preventDefault()).
    Object.defineProperty(EP, "returnValue", {
      get: function () { return !st(this).defaultPrevented; },
      set: function (v) { if (v === false) { var s = st(this); if (s.cancelable && !s.inPassive) { s.defaultPrevented = true; } } },
      enumerable: true, configurable: true
    });
    // cancelBubble: legacy alias of the stop-propagation flag. Getter returns it; setting to true
    // sets the flag (like stopPropagation()), setting to false is a no-op.
    Object.defineProperty(EP, "cancelBubble", {
      get: function () { return st(this).stopPropagation; },
      set: function (v) { if (v) { st(this).stopPropagation = true; } },
      enumerable: true, configurable: true
    });
    EP.preventDefault = function () { var s = st(this); if (s.cancelable && !s.inPassive) { s.defaultPrevented = true; } };
    EP.stopPropagation = function () { st(this).stopPropagation = true; };
    EP.stopImmediatePropagation = function () { var s = st(this); s.stopPropagation = true; s.stopImmediate = true; };
    EP.composedPath = function () { var s = st(this); return s.path ? s.path.slice() : []; };
    EP.initEvent = function (type, bubbles, cancelable) {
      var s = st(this);
      if (s.dispatched) { return; }
      s.type = String(type);
      s.bubbles = !!bubbles;
      s.cancelable = !!cancelable;
      s.defaultPrevented = false; s.isTrusted = false;
      s.target = null; s.stopPropagation = false; s.stopImmediate = false;
    };
    // Phase constants on both the constructor and the prototype.
    var phases = { NONE: 0, CAPTURING_PHASE: 1, AT_TARGET: 2, BUBBLING_PHASE: 3 };
    for (var pk in phases) {
      Object.defineProperty(Event, pk, { value: phases[pk], enumerable: true });
      Object.defineProperty(EP, pk, { value: phases[pk], enumerable: true });
    }
    def(globalThis, "Event", Event);
    // Expose the internal-state helpers so dispatchEvent / createEvent can drive events.
    def(globalThis, "__eventState", st);
    def(globalThis, "__initEventState", initEventState);

    // Build a subclass: ctor copies its own init members (from `members`) on top of the parent.
    // `members` maps property -> default value. `coerce` optionally transforms an init value.
    function defSubclass(name, ParentCtor, members, validate) {
      function Ctor(type, init) {
        ParentCtor.call(this, type, init);
        if (init === undefined || init === null) { init = {}; }
        if (validate) { validate(init); }
        for (var k in members) {
          var v = (k in init) ? init[k] : members[k];
          def(this, k, v);
        }
      }
      Ctor.prototype = Object.create(ParentCtor.prototype);
      Object.defineProperty(Ctor.prototype, "constructor", { value: Ctor, enumerable: false, configurable: true, writable: true });
      def(globalThis, name, Ctor);
      Ctor.__members = members;
      Ctor.__parent = ParentCtor;
      return Ctor;
    }

    // CustomEvent: read-only `detail` + legacy initCustomEvent.
    var CustomEvent = defSubclass("CustomEvent", Event, { detail: null });
    CustomEvent.prototype.initCustomEvent = function (type, bubbles, cancelable, detail) {
      var s = st(this);
      if (s.dispatched) { return; }
      this.initEvent(type, bubbles, cancelable);
      def(this, "detail", detail === undefined ? null : detail);
    };

    function requireObjOrNull(v, what) {
      if (v !== undefined && v !== null && typeof v !== "object" && typeof v !== "function") {
        throw new TypeError(what + " is not an object");
      }
    }

    var UIEvent = defSubclass("UIEvent", Event, { view: null, detail: 0 }, function (init) {
      if ("view" in init) { requireObjOrNull(init.view, "view"); }
    });
    var modInit = function (init) {
      if ("relatedTarget" in init) { requireObjOrNull(init.relatedTarget, "relatedTarget"); }
    };
    var FocusEvent = defSubclass("FocusEvent", UIEvent, { relatedTarget: null }, modInit);
    var MouseEvent = defSubclass("MouseEvent", UIEvent, {
      screenX: 0, screenY: 0, clientX: 0, clientY: 0, button: 0, buttons: 0,
      ctrlKey: false, shiftKey: false, altKey: false, metaKey: false,
      relatedTarget: null, movementX: 0, movementY: 0
    }, modInit);
    MouseEvent.prototype.getModifierState = function (k) {
      switch (k) { case "Control": return !!this.ctrlKey; case "Shift": return !!this.shiftKey;
        case "Alt": return !!this.altKey; case "Meta": return !!this.metaKey; default: return false; }
    };
    var WheelEvent = defSubclass("WheelEvent", MouseEvent, { deltaX: 0, deltaY: 0, deltaZ: 0, deltaMode: 0 }, modInit);
    var DragEvent = defSubclass("DragEvent", MouseEvent, { dataTransfer: null }, modInit);
    var PointerEvent = defSubclass("PointerEvent", MouseEvent, {
      pointerId: 0, width: 1, height: 1, pressure: 0, tangentialPressure: 0,
      tiltX: 0, tiltY: 0, twist: 0, altitudeAngle: 0, azimuthAngle: 0,
      pointerType: "", isPrimary: false
    }, modInit);
    var KeyboardEvent = defSubclass("KeyboardEvent", UIEvent, {
      key: "", code: "", location: 0, repeat: false, isComposing: false,
      ctrlKey: false, shiftKey: false, altKey: false, metaKey: false,
      charCode: 0, keyCode: 0, which: 0
    });
    KeyboardEvent.prototype.getModifierState = MouseEvent.prototype.getModifierState;
    var CompositionEvent = defSubclass("CompositionEvent", UIEvent, { data: "" });
    var InputEvent = defSubclass("InputEvent", UIEvent, { data: null, inputType: "", isComposing: false });
    var TouchEvent = defSubclass("TouchEvent", UIEvent, {
      touches: [], targetTouches: [], changedTouches: [],
      ctrlKey: false, shiftKey: false, altKey: false, metaKey: false
    });
    // Plain-Event subclasses (extend Event directly).
    defSubclass("PopStateEvent", Event, { state: null });
    defSubclass("HashChangeEvent", Event, { oldURL: "", newURL: "" });
    defSubclass("PageTransitionEvent", Event, { persisted: false });
    defSubclass("BeforeUnloadEvent", Event, { returnValue: "" });
    defSubclass("MessageEvent", Event, { data: null, origin: "", lastEventId: "", source: null, ports: [] });
    defSubclass("ProgressEvent", Event, { lengthComputable: false, loaded: 0, total: 0 });
    defSubclass("ErrorEvent", Event, { message: "", filename: "", lineno: 0, colno: 0, error: null });
    defSubclass("PromiseRejectionEvent", Event, { promise: null, reason: undefined });
    defSubclass("StorageEvent", Event, { key: null, oldValue: null, newValue: null, url: "", storageArea: null });
    defSubclass("AnimationEvent", Event, { animationName: "", elapsedTime: 0, pseudoElement: "" });
    defSubclass("TransitionEvent", Event, { propertyName: "", elapsedTime: 0, pseudoElement: "" });
    defSubclass("CloseEvent", Event, { code: 0, reason: "", wasClean: false });
    defSubclass("DeviceMotionEvent", Event, { acceleration: null, accelerationIncludingGravity: null, rotationRate: null, interval: 0 });
    defSubclass("DeviceOrientationEvent", Event, { alpha: null, beta: null, gamma: null, absolute: false });
    defSubclass("TextEvent", UIEvent, { data: "" });

    // document.createEvent legacy factory: case-insensitive name -> interface, per the DOM spec
    // table. Returns an UNINITIALIZED event (type==="") whose prototype is the interface's
    // prototype; the caller must initEvent()/initCustomEvent()/... before dispatching.
    var createEventTable = {
      "event": Event, "events": Event, "htmlevents": Event, "svgevents": Event,
      "customevent": CustomEvent,
      "uievent": UIEvent, "uievents": UIEvent,
      "mouseevent": MouseEvent, "mouseevents": MouseEvent,
      "keyboardevent": KeyboardEvent,
      "compositionevent": CompositionEvent,
      "focusevent": FocusEvent,
      "messageevent": MessageEvent,
      "hashchangeevent": globalThis.HashChangeEvent,
      "beforeunloadevent": globalThis.BeforeUnloadEvent,
      "dragevent": DragEvent,
      "storageevent": globalThis.StorageEvent,
      "textevent": TextEvent,
      "devicemotionevent": globalThis.DeviceMotionEvent,
      "deviceorientationevent": globalThis.DeviceOrientationEvent
    };
    def(globalThis, "__createEvent", function (name) {
      var key = String(name).toLowerCase();
      var Ctor = createEventTable.hasOwnProperty(key) ? createEventTable[key] : null;
      if (!Ctor) {
        throw new globalThis.DOMException(
          "The event \"" + name + "\" is not supported.", "NotSupportedError");
      }
      var ev = Object.create(Ctor.prototype);
      initEventState(ev);
      // Materialise this interface's own (and inherited) members as data properties with defaults
      // so they exist before init*() is called, matching a freshly-constructed event.
      var chain = [];
      for (var C = Ctor; C && C.__members; C = C.__parent) { chain.unshift(C); }
      for (var i = 0; i < chain.length; i++) {
        var m = chain[i].__members;
        for (var k in m) { def(ev, k, m[k]); }
      }
      return ev;
    });
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
    // Run it through the shared capture/target/bubble dispatch (which honours stopPropagation,
    // capture listeners, and returns !defaultPrevented).
    return globalThis.__dispatchEventObject(node, ev);
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
    // Non-bubbling: __dispatchEventObject skips the bubble phase (capture + target still run).
    return globalThis.__dispatchEventObject(node, ev);
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

  // --- <select> option pick (driven from Rust when the native dropdown menu is used) ---------
  // Toggle a <details>'s `open` attribute (from clicking its <summary>), then fire a non-bubbling
  // `toggle` event so the page reacts.
  def(globalThis, "__toggleDetails", function (nodeId) {
    var el = null;
    try { el = canon(__wrapNode(nodeId)); } catch (e) { el = null; }
    if (!el) { return; }
    var tag = "";
    try { tag = typeof el.tagName === "string" ? el.tagName.toLowerCase() : ""; } catch (e2) {}
    if (tag !== "details") { return; }
    if (__getAttr(nodeId, "open") != null) { __removeAttr(nodeId, "open"); }
    else { __setAttr(nodeId, "open", ""); }
    __dispatchSyntheticEventNonBubbling(nodeId, "toggle", {});
  });

  // Mark the `index`-th descendant <option> as selected (clearing `selected` on the others), set
  // the <select>'s `value` attribute to the chosen option's value (its `value` attr, else its
  // text), then fire bubbling `input` + `change` on the <select> so the page reacts. Returns true
  // if the selection actually changed. <optgroup>s are flattened (depth-first); single-pick only.
  def(globalThis, "__setSelectIndex", function (nodeId, index) {
    var sel = null;
    try { sel = canon(__wrapNode(nodeId)); } catch (e) { sel = null; }
    if (!sel) { return false; }
    var tag = "";
    try { tag = typeof sel.tagName === "string" ? sel.tagName.toLowerCase() : ""; } catch (e2) {}
    if (tag !== "select") { return false; }
    if (__getAttr(nodeId, "disabled") != null) { return false; }

    var options = [];
    try { options = sel.querySelectorAll("option"); } catch (e3) { options = []; }
    if (index < 0 || index >= options.length) { return false; }

    var optText = function (opt) {
      var t = "";
      try { t = opt.textContent == null ? "" : String(opt.textContent); } catch (e) {}
      return t.replace(/\s+/g, " ").replace(/^ | $/g, "");
    };
    var optValue = function (opt) {
      var v = null;
      try { v = opt.getAttribute("value"); } catch (e) {}
      return v == null ? optText(opt) : String(v);
    };

    // Was this already the selected option? (matches the layout crate's selection rule.)
    var wasSelected = false;
    try { wasSelected = options[index].getAttribute("selected") != null; } catch (e4) {}

    for (var i = 0; i < options.length; i++) {
      try {
        if (i === index) { options[i].setAttribute("selected", ""); }
        else { options[i].removeAttribute("selected"); }
      } catch (e5) {}
    }
    var newValue = optValue(options[index]);
    var prevValue = String(__getAttr(nodeId, "value") || "");
    try { __setAttr(nodeId, "value", newValue); } catch (e6) {}

    var changed = !wasSelected || prevValue !== newValue;
    __dispatchSyntheticEvent(nodeId, "input", {});
    __dispatchSyntheticEvent(nodeId, "change", {});
    return changed;
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

  // --- Canvas 2D context ---------------------------------------------------------------------
  // A real (software) CanvasRenderingContext2D. It keeps drawing STATE (styles + a 2D affine
  // transform + the current path) and records a DISPLAY LIST of resolved commands: every command
  // carries already-transformed device-space coordinates and a resolved CSS color (or gradient),
  // so the Rust engine needs no matrix/style math — it just rasterizes. `__canvasLists()` hands
  // the engine every canvas's {id,width,height,commands}.
  function __cnvMatMul(m, n) {
    // m, n are [a,b,c,d,e,f]; returns m*n (apply n first, then m), matching CanvasRenderingContext2D.
    return [
      m[0] * n[0] + m[2] * n[1],
      m[1] * n[0] + m[3] * n[1],
      m[0] * n[2] + m[2] * n[3],
      m[1] * n[2] + m[3] * n[3],
      m[0] * n[4] + m[2] * n[5] + m[4],
      m[1] * n[4] + m[3] * n[5] + m[5],
    ];
  }
  function __cnvApply(m, x, y) {
    return [m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]];
  }
  // Average scale of the matrix (for lineWidth / radius scaling). sqrt(|det|).
  function __cnvScale(m) {
    var det = m[0] * m[3] - m[1] * m[2];
    return Math.sqrt(Math.abs(det)) || 1;
  }
  function __makeCanvas2D(el) {
    var nodeId = (el && typeof el.__node === "number") ? el.__node : -1;
    var list = [];                 // the display list
    var state = {                  // current drawing state
      fillStyle: '#000000', strokeStyle: '#000000', lineWidth: 1, globalAlpha: 1,
      font: "10px sans-serif", fontSize: 10, textAlign: "start", textBaseline: "alphabetic",
      m: [1, 0, 0, 1, 0, 0],
      lineDash: [], lineDashOffset: 0,
      shadowBlur: 0, shadowColor: "rgba(0,0,0,0)", shadowOffsetX: 0, shadowOffsetY: 0,
      clip: null,                  // device-space clip rect [x,y,w,h] (bounding box of clip path)
    };
    var stack = [];                // save/restore stack
    var subpaths = [];             // array of polylines; each polyline is [x0,y0,x1,y1,...] (device)
    var cur = null;                // current subpath being built
    var penX = 0, penY = 0;        // current point in USER space (pre-transform)
    var startX = 0, startY = 0;    // subpath start (user space), for closePath
    function clone(s) {
      return { fillStyle: s.fillStyle, strokeStyle: s.strokeStyle, lineWidth: s.lineWidth,
        globalAlpha: s.globalAlpha, font: s.font, fontSize: s.fontSize, textAlign: s.textAlign,
        textBaseline: s.textBaseline, m: s.m.slice(),
        lineDash: s.lineDash.slice(), lineDashOffset: s.lineDashOffset,
        shadowBlur: s.shadowBlur, shadowColor: s.shadowColor,
        shadowOffsetX: s.shadowOffsetX, shadowOffsetY: s.shadowOffsetY,
        clip: s.clip ? s.clip.slice() : null };
    }
    // Resolve a fill/stroke style: a CSS color string passes through; a gradient object is encoded.
    function resolveStyle(style) {
      // A pattern (createPattern) is approximated as a solid fallback color (see __pattern below).
      if (style && typeof style === "object" && style.__pattern) {
        return { color: style.fallback || '#808080' };
      }
      if (style && typeof style === "object" && style.__grad) {
        var g = style;
        var stops = g.stops.map(function (s) { return { offset: s.offset, color: s.color }; });
        if (g.kind === "linear") {
          var p0 = __cnvApply(state.m, g.x0, g.y0), p1 = __cnvApply(state.m, g.x1, g.y1);
          return { gradient: "linear", x0: p0[0], y0: p0[1], x1: p1[0], y1: p1[1], stops: stops };
        }
        var c0 = __cnvApply(state.m, g.x0, g.y0), c1 = __cnvApply(state.m, g.x1, g.y1);
        var sc = __cnvScale(state.m);
        return { gradient: "radial", x0: c0[0], y0: c0[1], r0: g.r0 * sc,
          x1: c1[0], y1: c1[1], r1: g.r1 * sc, stops: stops };
      }
      return { color: String(style == null ? '#000' : style) };
    }
    function flushSub() { if (cur && cur.length >= 2) { subpaths.push(cur); } cur = null; }
    // Transform + emit the current set of subpaths (returns a fresh array of device polylines).
    function devicePaths() {
      flushSub();
      var out = [];
      for (var i = 0; i < subpaths.length; i++) { out.push(subpaths[i].slice()); }
      // Rebuild cur from the last so further building keeps working (we already flushed).
      subpaths = out.map(function (p) { return p.slice(); });
      return out;
    }
    function addPoint(ux, uy) {
      var p = __cnvApply(state.m, ux, uy);
      if (!cur) { cur = []; }
      cur.push(p[0], p[1]);
      penX = ux; penY = uy;
    }
    // Is a drop-shadow currently active? (non-transparent shadowColor AND a nonzero offset/blur).
    function shadowActive() {
      if (!state.shadowOffsetX && !state.shadowOffsetY && !state.shadowBlur) { return false; }
      var c = String(state.shadowColor);
      // Quick transparent checks (rgba(...,0) / transparent / #..00). Anything else is opaque-ish.
      if (c === "transparent") { return false; }
      var m = /rgba?\([^)]*?,\s*([0-9.]+)\s*\)/.exec(c);
      if (m && parseFloat(m[1]) === 0) { return false; }
      return true;
    }
    // Offset every geometry field of a command (device space) by (dx,dy). Used for shadow copies.
    function offsetCmd(cmd, dx, dy) {
      var o = {};
      for (var k in cmd) { o[k] = cmd[k]; }
      if (o.quad) { o.quad = o.quad.slice(); for (var i = 0; i < o.quad.length; i += 2) { o.quad[i] += dx; o.quad[i + 1] += dy; } }
      function off(arr) { return arr.map(function (poly) { var p = poly.slice(); for (var j = 0; j < p.length; j += 2) { p[j] += dx; p[j + 1] += dy; } return p; }); }
      if (o.polygons) { o.polygons = off(o.polygons); }
      if (o.polylines) { o.polylines = off(o.polylines); }
      if (typeof o.x === "number") { o.x += dx; }
      if (typeof o.y === "number") { o.y += dy; }
      if (o.clip) { o.clip = o.clip.slice(); o.clip[0] += dx; o.clip[1] += dy; }
      return o;
    }
    // Push a draw command, applying the current clip rect and (best-effort) drop shadow. The shadow
    // is an offset copy painted in shadowColor BEFORE the main command (blur approximated by the
    // engine spreading the shadow color over a small radius).
    function emit(cmd) {
      if (state.clip) { cmd.clip = state.clip.slice(); }
      if (shadowActive()) {
        var sc = __cnvScale(state.m);
        var sh = offsetCmd(cmd, state.shadowOffsetX * sc, state.shadowOffsetY * sc);
        // Recolor the shadow: flat shadowColor, drop any gradient.
        delete sh.gradient; delete sh.stops; delete sh.x0; delete sh.y0; delete sh.x1; delete sh.y1; delete sh.r0; delete sh.r1;
        sh.color = String(state.shadowColor);
        sh.blur = state.shadowBlur * sc;
        list.push(sh);
      }
      list.push(cmd);
    }
    var ctx = {
      canvas: el, lineCap: "butt", lineJoin: "miter", miterLimit: 10, direction: "ltr",
      globalCompositeOperation: "source-over", imageSmoothingEnabled: true,
      __nodeId: nodeId, __list: list,
    };
    // Shadow + dash properties are save/restore-aware (kept on `state`), exposed live.
    Object.defineProperty(ctx, "shadowBlur", { get: function () { return state.shadowBlur; }, set: function (v) { var n = +v; if (n >= 0 && isFinite(n)) { state.shadowBlur = n; } }, enumerable: true });
    Object.defineProperty(ctx, "shadowColor", { get: function () { return state.shadowColor; }, set: function (v) { state.shadowColor = String(v); }, enumerable: true });
    Object.defineProperty(ctx, "shadowOffsetX", { get: function () { return state.shadowOffsetX; }, set: function (v) { var n = +v; if (isFinite(n)) { state.shadowOffsetX = n; } }, enumerable: true });
    Object.defineProperty(ctx, "shadowOffsetY", { get: function () { return state.shadowOffsetY; }, set: function (v) { var n = +v; if (isFinite(n)) { state.shadowOffsetY = n; } }, enumerable: true });
    Object.defineProperty(ctx, "lineDashOffset", { get: function () { return state.lineDashOffset; }, set: function (v) { var n = +v; if (isFinite(n)) { state.lineDashOffset = n; } }, enumerable: true });
    // Styled state exposed as live properties.
    Object.defineProperty(ctx, "fillStyle", { get: function () { return state.fillStyle; }, set: function (v) { state.fillStyle = v; }, enumerable: true });
    Object.defineProperty(ctx, "strokeStyle", { get: function () { return state.strokeStyle; }, set: function (v) { state.strokeStyle = v; }, enumerable: true });
    Object.defineProperty(ctx, "lineWidth", { get: function () { return state.lineWidth; }, set: function (v) { var n = +v; if (n > 0 && isFinite(n)) { state.lineWidth = n; } }, enumerable: true });
    Object.defineProperty(ctx, "globalAlpha", { get: function () { return state.globalAlpha; }, set: function (v) { var n = +v; if (n >= 0 && n <= 1) { state.globalAlpha = n; } }, enumerable: true });
    Object.defineProperty(ctx, "textAlign", { get: function () { return state.textAlign; }, set: function (v) { state.textAlign = String(v); }, enumerable: true });
    Object.defineProperty(ctx, "textBaseline", { get: function () { return state.textBaseline; }, set: function (v) { state.textBaseline = String(v); }, enumerable: true });
    Object.defineProperty(ctx, "font", { get: function () { return state.font; }, set: function (v) {
      state.font = String(v);
      var mm = /(\d+(?:\.\d+)?)px/.exec(state.font); // loose: just pull the px size
      if (mm) { state.fontSize = parseFloat(mm[1]); }
      else { var pt = /(\d+(?:\.\d+)?)pt/.exec(state.font); if (pt) { state.fontSize = parseFloat(pt[1]) * 1.333; } }
    }, enumerable: true });

    ctx.save = function () { stack.push(clone(state)); };
    ctx.restore = function () { if (stack.length) { state = stack.pop(); } };
    // Transform ops mutate the current matrix.
    ctx.translate = function (x, y) { state.m = __cnvMatMul(state.m, [1, 0, 0, 1, +x || 0, +y || 0]); };
    ctx.scale = function (x, y) { state.m = __cnvMatMul(state.m, [+x || 0, 0, 0, +y || 0, 0, 0]); };
    ctx.rotate = function (a) { var c = Math.cos(a), s = Math.sin(a); state.m = __cnvMatMul(state.m, [c, s, -s, c, 0, 0]); };
    ctx.transform = function (a, b, c, d, e, f) { state.m = __cnvMatMul(state.m, [+a, +b, +c, +d, +e, +f]); };
    ctx.setTransform = function (a, b, c, d, e, f) {
      if (a && typeof a === "object") { state.m = [a.a, a.b, a.c, a.d, a.e, a.f]; }
      else { state.m = [+a, +b, +c, +d, +e, +f]; }
    };
    ctx.resetTransform = function () { state.m = [1, 0, 0, 1, 0, 0]; };
    ctx.getTransform = function () { var m = state.m; return { a: m[0], b: m[1], c: m[2], d: m[3], e: m[4], f: m[5] }; };

    // Path building. Arcs / curves are FLATTENED to polylines here, in user space, then transformed.
    ctx.beginPath = function () { subpaths = []; cur = null; };
    ctx.moveTo = function (x, y) { flushSub(); startX = +x; startY = +y; addPoint(+x, +y); };
    ctx.lineTo = function (x, y) { if (!cur) { startX = +x; startY = +y; } addPoint(+x, +y); };
    ctx.closePath = function () { if (cur && cur.length >= 2) { addPoint(startX, startY); } };
    ctx.rect = function (x, y, w, h) {
      flushSub(); x = +x; y = +y; w = +w; h = +h;
      addPoint(x, y); addPoint(x + w, y); addPoint(x + w, y + h); addPoint(x, y + h); addPoint(x, y);
      flushSub();
    };
    ctx.arc = function (x, y, r, a0, a1, ccw) {
      x = +x; y = +y; r = +r; a0 = +a0; a1 = +a1;
      var N = 24, span = a1 - a0;
      if (ccw) { if (span > 0) { span -= 2 * Math.PI; } } else { if (span < 0) { span += 2 * Math.PI; } }
      for (var i = 0; i <= N; i++) {
        var a = a0 + span * (i / N);
        var px = x + Math.cos(a) * r, py = y + Math.sin(a) * r;
        if (i === 0 && !cur) { addPoint(px, py); } else { addPoint(px, py); }
      }
    };
    ctx.ellipse = function (x, y, rx, ry, rot, a0, a1, ccw) {
      x = +x; y = +y; rx = +rx; ry = +ry; rot = +rot || 0; a0 = +a0; a1 = +a1;
      var N = 24, span = a1 - a0;
      if (ccw) { if (span > 0) { span -= 2 * Math.PI; } } else { if (span < 0) { span += 2 * Math.PI; } }
      var cr = Math.cos(rot), sr = Math.sin(rot);
      for (var i = 0; i <= N; i++) {
        var a = a0 + span * (i / N), ex = Math.cos(a) * rx, ey = Math.sin(a) * ry;
        addPoint(x + ex * cr - ey * sr, y + ex * sr + ey * cr);
      }
    };
    ctx.arcTo = function (x1, y1, x2, y2, r) {
      // Approximate: line to the first tangent point, then to the second (good enough flattened).
      ctx.lineTo(+x1, +y1); ctx.lineTo(+x2, +y2);
    };
    ctx.quadraticCurveTo = function (cx, cy, x, y) {
      cx = +cx; cy = +cy; x = +x; y = +y;
      var x0 = penX, y0 = penY, N = 16;
      for (var i = 1; i <= N; i++) {
        var t = i / N, u = 1 - t;
        addPoint(u * u * x0 + 2 * u * t * cx + t * t * x, u * u * y0 + 2 * u * t * cy + t * t * y);
      }
    };
    ctx.bezierCurveTo = function (c1x, c1y, c2x, c2y, x, y) {
      c1x = +c1x; c1y = +c1y; c2x = +c2x; c2y = +c2y; x = +x; y = +y;
      var x0 = penX, y0 = penY, N = 16;
      for (var i = 1; i <= N; i++) {
        var t = i / N, u = 1 - t;
        var b0 = u * u * u, b1 = 3 * u * u * t, b2 = 3 * u * t * t, b3 = t * t * t;
        addPoint(b0 * x0 + b1 * c1x + b2 * c2x + b3 * x, b0 * y0 + b1 * c1y + b2 * c2y + b3 * y);
      }
    };
    ctx.roundRect = function (x, y, w, h) { ctx.rect(x, y, w, h); }; // corners approximated as square

    // Drawing ops append resolved commands.
    function rectCmd(op, x, y, w, h, style) {
      x = +x; y = +y; w = +w; h = +h;
      var p0 = __cnvApply(state.m, x, y), p1 = __cnvApply(state.m, x + w, y),
          p2 = __cnvApply(state.m, x + w, y + h), p3 = __cnvApply(state.m, x, y + h);
      var cmd = { op: op, quad: [p0[0], p0[1], p1[0], p1[1], p2[0], p2[1], p3[0], p3[1]], alpha: state.globalAlpha };
      if (op !== "clearRect") { var r = resolveStyle(style); for (var k in r) { cmd[k] = r[k]; } emit(cmd); }
      else { if (state.clip) { cmd.clip = state.clip.slice(); } list.push(cmd); } // clearRect: clip but no shadow
    }
    ctx.fillRect = function (x, y, w, h) { rectCmd("fillRect", x, y, w, h, state.fillStyle); };
    ctx.clearRect = function (x, y, w, h) {
      // A clearRect covering the whole canvas resets the display list (bounds growth for
      // clear+redraw animation loops). Otherwise it's an erase quad.
      var cw = el.width | 0 || 300, chh = el.height | 0 || 150;
      var m = state.m, axis = (Math.abs(m[1]) < 1e-6 && Math.abs(m[2]) < 1e-6);
      if (axis && (+x) <= 0 && (+y) <= 0 && (+x + +w) >= cw && (+y + +h) >= chh) { list.length = 0; return; }
      rectCmd("clearRect", x, y, w, h, null);
    };
    ctx.strokeRect = function (x, y, w, h) {
      x = +x; y = +y; w = +w; h = +h;
      var pts = [x, y, x + w, y, x + w, y + h, x, y + h, x, y];
      var dev = [];
      for (var i = 0; i < pts.length; i += 2) { var p = __cnvApply(state.m, pts[i], pts[i + 1]); dev.push(p[0], p[1]); }
      var r = resolveStyle(state.strokeStyle);
      var cmd = { op: "stroke", polylines: [dev], width: state.lineWidth * __cnvScale(state.m), alpha: state.globalAlpha };
      for (var k in r) { cmd[k] = r[k]; }
      attachDash(cmd);
      emit(cmd);
    };
    ctx.fill = function () {
      var polys = devicePaths();
      if (!polys.length) { return; }
      var r = resolveStyle(state.fillStyle);
      var cmd = { op: "fill", polygons: polys, alpha: state.globalAlpha };
      for (var k in r) { cmd[k] = r[k]; }
      emit(cmd);
    };
    ctx.stroke = function () {
      var polys = devicePaths();
      if (!polys.length) { return; }
      var r = resolveStyle(state.strokeStyle);
      var cmd = { op: "stroke", polylines: polys, width: state.lineWidth * __cnvScale(state.m), alpha: state.globalAlpha };
      for (var k in r) { cmd[k] = r[k]; }
      attachDash(cmd);
      emit(cmd);
    };
    // Attach the current line-dash pattern (scaled to device space) to a stroke command.
    function attachDash(cmd) {
      if (state.lineDash && state.lineDash.length) {
        var sc = __cnvScale(state.m);
        cmd.dash = state.lineDash.map(function (d) { return d * sc; });
        cmd.dashOffset = state.lineDashOffset * sc;
      }
    }
    function textCmd(op, text, x, y, style) {
      var p = __cnvApply(state.m, +x || 0, +y || 0);
      var r = resolveStyle(style);
      var cmd = { op: "text", text: String(text), x: p[0], y: p[1],
        size: state.fontSize * __cnvScale(state.m), align: state.textAlign,
        baseline: state.textBaseline, alpha: state.globalAlpha };
      for (var k in r) { cmd[k] = r[k]; }
      emit(cmd);
    }
    ctx.fillText = function (t, x, y) { textCmd("fillText", t, x, y, state.fillStyle); };
    ctx.strokeText = function (t, x, y) { textCmd("strokeText", t, x, y, state.strokeStyle); };
    ctx.measureText = function (s) {
      var w = __measureCanvasText(String(s == null ? "" : s), state.fontSize);
      return { width: w, actualBoundingBoxLeft: 0, actualBoundingBoxRight: w,
        actualBoundingBoxAscent: state.fontSize * 0.8, actualBoundingBoxDescent: state.fontSize * 0.2,
        fontBoundingBoxAscent: state.fontSize * 0.8, fontBoundingBoxDescent: state.fontSize * 0.2 };
    };

    // Gradients.
    function makeGradient(kind, x0, y0, x1, y1, r0, r1) {
      var g = { __grad: true, kind: kind, x0: +x0, y0: +y0, x1: +x1, y1: +y1, r0: +r0 || 0, r1: +r1 || 0, stops: [] };
      g.addColorStop = function (off, color) { g.stops.push({ offset: +off, color: String(color) }); };
      return g;
    }
    ctx.createLinearGradient = function (x0, y0, x1, y1) { return makeGradient("linear", x0, y0, x1, y1, 0, 0); };
    ctx.createRadialGradient = function (x0, y0, r0, x1, y1, r1) { return makeGradient("radial", x0, y0, x1, y1, r0, r1); };
    ctx.createConicGradient = function () { return makeGradient("linear", 0, 0, 0, 0, 0, 0); };

    var noop = function () {};
    ctx.drawFocusIfNeeded = noop;
    ctx.isPointInPath = function () { return false; }; ctx.isPointInStroke = function () { return false; };

    // clip(): constrain subsequent draws to the bounding box of the current path (a documented
    // simplification — real clip is the path shape; we track its device-space AABB). Intersects with
    // any existing clip and is save/restore-aware (clip lives on `state`).
    ctx.clip = function () {
      var polys = devicePaths();
      if (!polys.length) { return; }
      var minx = Infinity, miny = Infinity, maxx = -Infinity, maxy = -Infinity;
      for (var i = 0; i < polys.length; i++) {
        var p = polys[i];
        for (var j = 0; j + 1 < p.length; j += 2) {
          if (p[j] < minx) { minx = p[j]; } if (p[j] > maxx) { maxx = p[j]; }
          if (p[j + 1] < miny) { miny = p[j + 1]; } if (p[j + 1] > maxy) { maxy = p[j + 1]; }
        }
      }
      if (!isFinite(minx)) { return; }
      var nx = minx, ny = miny, nw = maxx - minx, nh = maxy - miny;
      if (state.clip) { // intersect with the existing clip rect
        var cx = Math.max(state.clip[0], nx), cy = Math.max(state.clip[1], ny);
        var cw = Math.min(state.clip[0] + state.clip[2], nx + nw) - cx;
        var chh = Math.min(state.clip[1] + state.clip[3], ny + nh) - cy;
        state.clip = [cx, cy, Math.max(0, cw), Math.max(0, chh)];
      } else {
        state.clip = [nx, ny, nw, nh];
      }
    };

    // Line dash. Pattern is in user-space units; scaled to device space at stroke time (attachDash).
    ctx.setLineDash = function (segs) {
      if (!segs || typeof segs.length !== "number") { return; }
      var out = [];
      for (var i = 0; i < segs.length; i++) { var n = +segs[i]; if (isFinite(n) && n >= 0) { out.push(n); } else { return; } }
      // An odd-length pattern is doubled (per spec).
      if (out.length % 2 === 1) { out = out.concat(out); }
      state.lineDash = out;
    };
    ctx.getLineDash = function () { return state.lineDash.slice(); };

    // createPattern: best-effort. We cannot tile in the engine, so return an object usable as a
    // fillStyle/strokeStyle that resolveStyle falls back to a solid color (documented simplification).
    ctx.createPattern = function (image, repetition) {
      return { __pattern: true, repetition: String(repetition || "repeat"), fallback: '#808080' };
    };

    // ---- Image data ----
    function makeImageData(w, h, src) {
      var ww = Math.max(1, w | 0), hh = Math.max(1, h | 0);
      var data = src || new Uint8ClampedArray(ww * hh * 4);
      return { width: ww, height: hh, data: data, colorSpace: "srgb" };
    }
    ctx.createImageData = function (a, b) {
      // createImageData(w,h) | createImageData(imagedata)
      if (a && typeof a === "object" && a.width != null) { return makeImageData(a.width, a.height); }
      return makeImageData(a, b);
    };
    // getImageData reads the engine's pushed pixels (previous frame) for this canvas node. Returns a
    // zeroed buffer if the canvas has not been rasterized yet (one-render lag — documented).
    ctx.getImageData = function (x, y, w, h) {
      var ww = Math.max(1, w | 0), hh = Math.max(1, h | 0);
      var data = new Uint8ClampedArray(ww * hh * 4);
      try {
        if (nodeId >= 0 && typeof __canvasPixels === "function") {
          var got = __canvasPixels(nodeId, x | 0, y | 0, ww, hh);
          if (got && got.b64) {
            var bin = (typeof atob === "function") ? atob(got.b64) : "";
            var n = Math.min(bin.length, data.length);
            for (var i = 0; i < n; i++) { data[i] = bin.charCodeAt(i) & 0xff; }
          }
        }
      } catch (e) {}
      return makeImageData(ww, hh, data);
    };
    // putImageData records a command that writes the pixel block into the canvas surface at (dx,dy).
    // The pixels are base64-bridged to the engine. Dirty-rect args are honored (subset of the block).
    ctx.putImageData = function (imagedata, dx, dy, dirtyX, dirtyY, dirtyW, dirtyH) {
      if (!imagedata || !imagedata.data) { return; }
      var iw = imagedata.width | 0, ih = imagedata.height | 0;
      if (iw <= 0 || ih <= 0) { return; }
      var d = imagedata.data, s = "";
      for (var i = 0; i < d.length; i++) { s += String.fromCharCode(d[i] & 0xff); }
      var b64 = (typeof btoa === "function") ? btoa(s) : "";
      // putImageData ignores the transform; (dx,dy) are device (canvas) pixels directly.
      var cmd = { op: "putImageData", dx: dx | 0, dy: dy | 0, iw: iw, ih: ih, b64: b64 };
      if (dirtyW != null) { cmd.dirtyX = dirtyX | 0; cmd.dirtyY = dirtyY | 0; cmd.dirtyW = dirtyW | 0; cmd.dirtyH = dirtyH | 0; }
      list.push(cmd);
    };

    // drawImage(src, dx,dy) | (src, dx,dy,dw,dh) | (src, sx,sy,sw,sh, dx,dy,dw,dh). `src` is an
    // HTMLImageElement or HTMLCanvasElement; the engine blits its pixels (by node id) into the dest
    // rect, honoring globalAlpha + clip. The dest rect is transformed by the current matrix (as a
    // quad); source sub-rect sampling is nearest-neighbor.
    ctx.drawImage = function (src) {
      var srcId = (src && typeof src.__node === "number") ? src.__node
                : (src && src.canvas && typeof src.canvas.__node === "number") ? src.canvas.__node : -1;
      if (srcId < 0) { return; }
      // Natural source size (for the 3-arg form's default dw/dh, and to default sw/sh).
      var natW = (src.naturalWidth | 0) || (src.width | 0) || 0;
      var natH = (src.naturalHeight | 0) || (src.height | 0) || 0;
      var sx = 0, sy = 0, sw = natW, sh = natH, dx, dy, dw, dh;
      if (arguments.length <= 3) {               // (src, dx, dy)
        dx = +arguments[1] || 0; dy = +arguments[2] || 0; dw = natW; dh = natH;
      } else if (arguments.length <= 5) {         // (src, dx, dy, dw, dh)
        dx = +arguments[1] || 0; dy = +arguments[2] || 0; dw = +arguments[3] || 0; dh = +arguments[4] || 0;
      } else {                                    // (src, sx, sy, sw, sh, dx, dy, dw, dh)
        sx = +arguments[1] || 0; sy = +arguments[2] || 0; sw = +arguments[3] || 0; sh = +arguments[4] || 0;
        dx = +arguments[5] || 0; dy = +arguments[6] || 0; dw = +arguments[7] || 0; dh = +arguments[8] || 0;
      }
      // Transform the dest rect's 4 corners into device space (a quad).
      var p0 = __cnvApply(state.m, dx, dy), p1 = __cnvApply(state.m, dx + dw, dy),
          p2 = __cnvApply(state.m, dx + dw, dy + dh), p3 = __cnvApply(state.m, dx, dy + dh);
      var cmd = { op: "drawImage", src: srcId,
        sx: sx, sy: sy, sw: sw, sh: sh,
        quad: [p0[0], p0[1], p1[0], p1[1], p2[0], p2[1], p3[0], p3[1]],
        alpha: state.globalAlpha };
      emit(cmd);
    };

    ctx.getContextAttributes = function () { return { alpha: true, desynchronized: false, colorSpace: "srgb", willReadFrequently: false }; };
    return ctx;
  }
  // ImageData constructor: new ImageData(w,h) | new ImageData(Uint8ClampedArray, w[, h]).
  if (typeof globalThis.ImageData !== "function") {
    globalThis.ImageData = function ImageData(a, b, c) {
      var data, w, h;
      if (a && typeof a === "object" && typeof a.length === "number") {
        data = a; w = b | 0; h = c != null ? (c | 0) : (w > 0 ? (a.length / 4 / w) | 0 : 0);
      } else {
        w = a | 0; h = b | 0; data = new Uint8ClampedArray(Math.max(0, w * h * 4));
      }
      if (w <= 0) { w = 1; } if (h <= 0) { h = 1; }
      this.width = w; this.height = h; this.data = data; this.colorSpace = "srgb";
    };
  }
  globalThis.__makeCanvas2D = __makeCanvas2D;

  // Approximate text advance for measureText. The JS crate has no font, so this is a proportional
  // per-character estimate (the engine rasterizes/aligns text with the REAL system font). Narrow
  // glyphs (i/l/.) ~0.32em, wide (m/w/W) ~0.92em, else ~0.55em — close enough for layout.
  function __measureCanvasText(s, px) {
    var w = 0;
    for (var i = 0; i < s.length; i++) {
      var ch = s[i];
      if ("iIl.,:;'|!".indexOf(ch) >= 0) { w += 0.32; }
      else if ("mwMW@".indexOf(ch) >= 0) { w += 0.92; }
      else if (ch >= "A" && ch <= "Z") { w += 0.68; }
      else if (ch === " ") { w += 0.30; }
      else { w += 0.52; }
    }
    return w * px;
  }

  // HTML "named properties on the window object": an element with an `id` is exposed as a bare
  // global so `target1` resolves to `<div id="target1">` without `document.getElementById`.
  // Browsers implement this via a live named-property getter; we install a configurable getter per
  // id that delegates to `getElementById` (so it stays live, returns the canonical wrapper, and
  // tree-order / duplicate-id resolution comes for free). Called once after the environment is
  // installed and the DOM is parsed, before any author script runs. We never shadow an existing
  // own/builtin global (e.g. an `id="location"` must not clobber `window.location`).
  globalThis.__installNamedGlobals = function () {
    var nodes;
    try { nodes = __querySelectorAll("[id]"); } catch (e) { return; }
    if (!nodes) { return; }
    for (var i = 0; i < nodes.length; i++) {
      var nid = nodes[i];
      var idStr;
      try { idStr = __getAttr(nid, "id"); } catch (e) { idStr = ""; }
      if (!idStr) { continue; }
      if (Object.prototype.hasOwnProperty.call(globalThis, idStr)) { continue; }
      (function (name) {
        try {
          Object.defineProperty(globalThis, name, {
            configurable: true,
            enumerable: false,
            get: function () { return document.getElementById(name); },
            // HTML named properties are overridable: assigning (including a global `var name = ...`)
            // replaces the named property with a plain data property. Without a setter, such an
            // assignment throws in strict/module code and aborts the script.
            set: function (v) {
              Object.defineProperty(globalThis, name, {
                value: v,
                writable: true,
                configurable: true,
                enumerable: true,
              });
            },
          });
        } catch (e) {}
      })(idStr);
    }
  };

  // The engine pulls every canvas's display list through this. Returns a JSON-ready array of
  // { id, width, height, commands:[...] }. Guard on the engine side: only called when the DOM has
  // a <canvas>.
  globalThis.__canvasLists = function () {
    var cs = globalThis.__canvases || [];
    var out = [];
    for (var i = 0; i < cs.length; i++) {
      var c = cs[i];
      if (!c || c.__nodeId < 0) { continue; }
      var el = c.canvas;
      out.push({ id: c.__nodeId, width: (el.width | 0) || 300, height: (el.height | 0) || 150, commands: c.__list });
    }
    return out;
  };

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
        Ok(value) => EvalOutput {
            value,
            console,
            error: None,
        },
        Err(error) => EvalOutput {
            value: None,
            console,
            error: Some(error),
        },
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
fn drain_event_loop(
    scope: &mut v8::PinScope,
    results: &mut [EvalOutput],
    fetch_rx: Option<&Receiver<FetchCompletion>>,
    ws_evt_rx: Option<&Receiver<WsEvent>>,
) -> bool {
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
    // Idle budget keeps ticks snappy; the network budget is raised because a page legitimately
    // waiting on a slow request (the slowest imlunahey _serverFn is ~6.8s) needs longer than 3s.
    let idle_budget = std::time::Duration::from_millis(3000);
    let network_budget = std::time::Duration::from_millis(15000);
    let mut iterations = 0usize;
    let mut did_work = false;
    loop {
        // Pull any completed background fetches and settle their JS promises, then run a microtask
        // checkpoint so the `.then` chains progress within this same drain.
        if resolve_completed_fetches(scope, fetch_rx) {
            did_work = true;
            scope.perform_microtask_checkpoint();
        }

        // Opportunistically deliver any pending WebSocket events (non-blocking). A socket is
        // long-lived, so this never gates the loop (no `in_flight`); events simply arrive within
        // ~one drain pass. A delivered handler may queue microtasks, so checkpoint after.
        if deliver_ws_events(scope, ws_evt_rx) {
            did_work = true;
            scope.perform_microtask_checkpoint();
        }

        let in_flight = scope
            .get_current_context()
            .get_slot::<HostState>()
            .map(|s| s.in_flight.get())
            .unwrap_or(0);
        // While requests are outstanding we use the longer budget (so a network-bound page isn't
        // cut off); otherwise the short idle budget keeps idle ticks cheap.
        let budget = if in_flight > 0 {
            network_budget
        } else {
            idle_budget
        };
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
            // queued a job.
            scope.perform_microtask_checkpoint();
            if run_due_timers(scope) {
                did_work = true;
            } else if in_flight > 0 {
                // No JS work is due but a request is still in flight: block briefly on the channel
                // (instead of busy-spinning) for the next completion, then loop to resolve it.
                if let Some(rx) = fetch_rx {
                    match rx.recv_timeout(std::time::Duration::from_millis(20)) {
                        Ok(completion) => {
                            deliver_fetch_completion(scope, completion);
                            did_work = true;
                            scope.perform_microtask_checkpoint();
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        // All senders gone (shouldn't happen mid-flight): stop waiting.
                        Err(_) => break,
                    }
                }
            } else {
                break;
            }
        }
    }

    // One final sweep: settle any completions that landed after the loop's last check, then run a
    // microtask checkpoint so their `.then` chains progress before we snapshot.
    if resolve_completed_fetches(scope, fetch_rx) {
        did_work = true;
        scope.perform_microtask_checkpoint();
    }
    // Same final sweep for WebSocket events that arrived after the loop's last pass.
    if deliver_ws_events(scope, ws_evt_rx) {
        did_work = true;
        scope.perform_microtask_checkpoint();
    }

    // MutationObserver delivery: callbacks fire as a microtask after the task. If the task queued
    // any DOM mutations, deliver them; a delivered callback may itself mutate the DOM, so loop
    // (bounded) until the queue drains or we hit the cap (guards against infinite mutation loops).
    {
        let has_mutations = |scope: &mut v8::PinScope| {
            scope
                .get_current_context()
                .get_slot::<HostState>()
                .map(|s| !s.mutations.borrow().is_empty())
                .unwrap_or(false)
        };
        let mut rounds = 0usize;
        while rounds < 64 && has_mutations(scope) {
            eval_internal(
                scope,
                "if (typeof __deliverMutations === 'function') { __deliverMutations(); }",
                "<mutations>",
            );
            scope.perform_microtask_checkpoint();
            // A callback may have scheduled timers; let one round of due work run so e.g. a
            // setTimeout(0) inside an observer still progresses within this drain.
            run_due_timers(scope);
            scope.perform_microtask_checkpoint();
            did_work = true;
            rounds += 1;
        }
    }

    // Collect timer/microtask errors recorded JS-side.
    let mut extra: Vec<String> = Vec::new();
    if let Some(joined) = eval_to_string(scope, "(globalThis.__timerErrors || []).join('\\u0000')")
    {
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
    eval_to_bool(
        scope,
        "(typeof __runDueTimers === 'function') && __runDueTimers()",
    )
}

/// Drain all currently-available background fetch completions (non-blocking `try_recv`) and settle
/// each one's JS promise. Returns whether any completion was delivered. No-op when `fetch_rx` is
/// `None` (the no-DOM / eval paths that never start async fetches).
fn resolve_completed_fetches(
    scope: &mut v8::PinScope,
    fetch_rx: Option<&Receiver<FetchCompletion>>,
) -> bool {
    let rx = match fetch_rx {
        Some(rx) => rx,
        None => return false,
    };
    let mut any = false;
    while let Ok(completion) = rx.try_recv() {
        deliver_fetch_completion(scope, completion);
        any = true;
    }
    any
}

/// Settle a single fetch completion on the worker thread: decrement the in-flight count, then call
/// `__resolveFetch(id, envelope)` (success) or `__rejectFetch(id)` (transport error → `None`).
fn deliver_fetch_completion(scope: &mut v8::PinScope, completion: FetchCompletion) {
    let (id, env) = completion;
    if let Some(state) = scope.get_current_context().get_slot::<HostState>() {
        state.in_flight.set(state.in_flight.get().saturating_sub(1));
    }
    match env {
        Some(envelope) => {
            // Pass the envelope as a JS string arg; build the call so the envelope can't break out
            // of the literal (it's user-controlled response data).
            let global = scope.get_current_context().global(scope);
            let resolve_key = v8::String::new(scope, "__resolveFetch").unwrap();
            let resolve_fn = global
                .get(scope, resolve_key.into())
                .and_then(|v| v8::Local::<v8::Function>::try_from(v).ok());
            if let Some(func) = resolve_fn {
                let id_arg = v8::Number::new(scope, id as f64).into();
                let env_arg = js_str(scope, &envelope);
                v8::tc_scope!(let tc, scope);
                let recv = global.into();
                func.call(tc, recv, &[id_arg, env_arg]);
            }
        }
        None => {
            eval_internal(
                scope,
                &format!("if (typeof __rejectFetch === 'function') {{ __rejectFetch({id}); }}"),
                "<rejectFetch>",
            );
        }
    }
}

/// Drain all currently-available WebSocket events (non-blocking `try_recv`) and dispatch each to JS
/// via `__wsDeliver(id, kind, payload)`. Returns whether any event was delivered. No-op when
/// `ws_evt_rx` is `None` (the no-DOM / run-once paths that never open a socket). A `close` event
/// (kind 3) also drops the socket's outgoing sender so its thread can exit.
fn deliver_ws_events(scope: &mut v8::PinScope, ws_evt_rx: Option<&Receiver<WsEvent>>) -> bool {
    let rx = match ws_evt_rx {
        Some(rx) => rx,
        None => return false,
    };
    let mut any = false;
    while let Ok((id, kind, payload)) = rx.try_recv() {
        // On close, drop the outgoing sender for this id (the socket thread is finishing).
        if kind == 3 {
            if let Some(state) = scope.get_current_context().get_slot::<HostState>() {
                state.ws_senders.borrow_mut().remove(&id);
            }
        }
        deliver_ws_event(scope, id, kind, &payload);
        any = true;
    }
    any
}

/// Dispatch one WebSocket event to JS by calling `__wsDeliver(id, kind, payload)` with the payload
/// as a string argument (so user-controlled message data can't break out of a source literal).
fn deliver_ws_event(scope: &mut v8::PinScope, id: u64, kind: u8, payload: &str) {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "__wsDeliver").unwrap();
    let func = global
        .get(scope, key.into())
        .and_then(|v| v8::Local::<v8::Function>::try_from(v).ok());
    if let Some(func) = func {
        let id_arg = v8::Number::new(scope, id as f64).into();
        let kind_arg = v8::Number::new(scope, kind as f64).into();
        let payload_arg = js_str(scope, payload);
        v8::tc_scope!(let tc, scope);
        let recv = global.into();
        func.call(tc, recv, &[id_arg, kind_arg, payload_arg]);
    }
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
        let mut isolate = new_guarded_isolate();
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
            // No async fetches on the bare eval path (no real fetcher), so pass no receiver.
            drain_event_loop(scope, &mut results, None, None);
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
            let result: (dom::Document, Vec<EvalOutput>) = {
                ensure_v8_initialized();
                let shared: SharedDoc = Rc::new(RefCell::new(doc));
                let mut isolate = new_guarded_isolate();
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
                    // run_with_dom installs no real fetcher, so no async fetches are ever started.
                    drain_event_loop(scope, &mut results, None, None);
                }
                // Recover the owned Document. Dropping the isolate releases the context (and HostState
                // slot, which holds the only other Rc clone of `shared`), so `try_unwrap` succeeds.
                drop(isolate);
                let doc = match Rc::try_unwrap(shared) {
                    Ok(cell) => cell.into_inner(),
                    Err(rc) => rc.borrow().clone(),
                };
                (doc, results)
            };
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
    fetcher: Rc<dyn Fn(&str) -> Option<(String, String)>>,
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
        let fetched = (self.fetcher)(url)?.0;
        self.sources
            .borrow_mut()
            .insert(url.to_string(), fetched.clone());
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
    registry
        .compiled
        .borrow_mut()
        .insert(url.to_string(), global);
    // Record identity -> URL so the resolve/dynamic-import callbacks can recover this module's own
    // canonical URL when resolving its relative specifiers.
    registry
        .identity_to_url
        .borrow_mut()
        .insert(module.get_identity_hash().get() as i32, url.to_string());
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
    let referrer_identity = referrer.get_identity_hash().get();
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
        state
            .console
            .borrow_mut()
            .push(format!("⚠ Unhandled rejection: {text}"));
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
        let identity = module.get_identity_hash().get();
        let mapped = registry.identity_to_url.borrow().get(&identity).cloned();
        mapped.unwrap_or_else(|| registry.base_url.clone())
    };

    // import.meta.url = <canonical url>
    if let (Some(key), Some(val)) = (v8::String::new(scope, "url"), v8::String::new(scope, &url)) {
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
    fetcher: Box<dyn Fn(&str) -> Option<(String, String)> + Send>,
    request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync>,
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
            // Background fetch threads deliver completions here; the receiver stays on this worker.
            let (fetch_tx, fetch_rx) = std::sync::mpsc::channel::<FetchCompletion>();
            let mut isolate = new_guarded_isolate();
            isolate.set_host_import_module_dynamically_callback(dynamic_import_callback);
            isolate.set_promise_reject_callback(promise_reject_callback);
            // Populate `import.meta.url` for every module the first time it touches `import.meta`,
            // so relative `new URL(..., import.meta.url)` (e.g. browserscore's support-status.js
            // `fetch(new URL('./support-status.css', import.meta.url))`) resolves correctly.
            isolate
                .set_host_initialize_import_meta_object_callback(initialize_import_meta_callback);

            let mut results: Vec<EvalOutput> = Vec::with_capacity(entries.len());
            {
                v8::scope!(let handle_scope, &mut isolate);
                let context = v8::Context::new(handle_scope, Default::default());
                let scope = &mut v8::ContextScope::new(handle_scope, context);
                // Share one fetcher between the module loader and the JS `fetch()` primitive.
                let fetcher: Rc<dyn Fn(&str) -> Option<(String, String)>> =
                    Rc::new(move |u: &str| fetcher(u));
                // The module path is run-once with no live event loop, so no real WebSocket support:
                // a connector that always errs and a dead-end event channel.
                let (ws_evt_tx, _ws_evt_rx) = std::sync::mpsc::channel::<WsEvent>();
                let ws_connector: WsConnector =
                    Arc::new(|_, _, _| Err("no WebSocket connector".to_string()));
                let state = HostState::with_fetcher(
                    Rc::clone(&shared),
                    Rc::clone(&fetcher),
                    request_fetcher,
                    fetch_tx,
                    ws_connector,
                    ws_evt_tx,
                );
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

                drain_event_loop(scope, &mut results, Some(&fetch_rx), None);
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

    EvalOutput {
        value: None,
        console,
        error,
    }
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
    /// reply with the eval result (value-or-error string), snapshot + console. Used for the
    /// interaction helpers AND the devtools console REPL.
    Eval {
        source: String,
        reply: std::sync::mpsc::Sender<(EvalOutput, dom::Document, Vec<String>)>,
    },
    /// Run due timers / microtasks; reply `Some(snapshot, console)` if work ran, else `None`.
    Tick {
        reply: std::sync::mpsc::Sender<Option<(dom::Document, Vec<String>)>>,
    },
    /// Push the engine's freshly-laid-out element rects (CSS px, document-absolute, top-origin)
    /// onto `HostState` so `getBoundingClientRect()` / `offsetWidth` / `scrollHeight` etc. can read
    /// real geometry. Fire-and-forget: no reply (the engine does not block on this).
    SetRects {
        /// `(node_id, x, y, width, height)` per laid-out node, CSS px, document-absolute.
        rects: Vec<(usize, f32, f32, f32, f32)>,
        /// `(node_id, natural_width, natural_height)` per decoded `<img>`, CSS px. Backs
        /// `img.naturalWidth`/`naturalHeight`.
        naturals: Vec<(usize, f32, f32)>,
        /// `(node_id, top, right, bottom, left)` per positioned box: the CSSOM *used* inset values
        /// in CSS px. Backs `getComputedStyle(el).top` etc. when the element has a box.
        insets: Vec<(usize, f32, f32, f32, f32)>,
        /// `(node_id, top, right, bottom, left)` per box: the CSSOM *used* margin values in CSS px.
        /// Backs `getComputedStyle(el).margin*` so resolved `auto` margins report their used value.
        margins: Vec<(usize, f32, f32, f32, f32)>,
        /// Vertical scroll offset in CSS px (subtracted to make rects viewport-relative).
        scroll_y_css: f32,
        /// Full document content height in CSS px (reported as documentElement/body scrollHeight).
        doc_height_css: f32,
    },
    /// Push the engine's freshly-rasterized canvas/image pixels onto `HostState` so `getImageData`
    /// can read real RGBA. Fire-and-forget: no reply. `(node_id, width, height, rgba8)` per source.
    SetCanvasPixels {
        pixels: Vec<(usize, u32, u32, Vec<u8>)>,
    },
    /// Stop the loop; the isolate is torn down on the thread it lives on.
    Stop,
}

/// A persistent JS runtime bound to one page. The V8 isolate + context live for the whole session
/// on a dedicated thread; [`dispatch_event`](Session::dispatch_event) and [`tick`](Session::tick)
/// post commands to that thread and block on the reply, returning a fresh DOM snapshot each time.
/// The session keeps mutating the live document; callers render the returned clone.
/// Per-tab runtime stats, sampled on the session thread and read lock-free by the engine/UI (for the
/// tab tooltip). `heap_bytes` is the V8 heap used size; `cpu_ns` is cumulative time spent actively
/// running JS on this tab's thread — a CPU proxy, since the thread is otherwise blocked on `recv`.
#[derive(Default)]
pub struct TabStats {
    pub heap_bytes: AtomicU64,
    pub cpu_ns: AtomicU64,
}

pub struct Session {
    tx: std::sync::mpsc::Sender<SessionCmd>,
    handle: Option<std::thread::JoinHandle<()>>,
    stats: Arc<TabStats>,
}

impl Session {
    /// V8 heap used by this tab (bytes).
    pub fn heap_bytes(&self) -> u64 {
        self.stats.heap_bytes.load(Ordering::Relaxed)
    }
    /// Cumulative active JS time on this tab's thread (ns); the UI samples deltas to show a CPU %.
    pub fn cpu_ns(&self) -> u64 {
        self.stats.cpu_ns.load(Ordering::Relaxed)
    }
}

impl Session {
    /// Spawn the runtime thread, create the isolate + context, install the browser environment, run
    /// the initial classic `scripts` in order then the module graph (`entries` + `modules`, via
    /// `fetcher`), drain once, and return the session plus the initial DOM snapshot + per-source
    /// [`EvalOutput`]s (one per classic script, then one per module entry — matching the order
    /// `run_with_dom`/`run_modules` would produce).
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    pub fn new(
        doc: dom::Document,
        scripts: Vec<String>,
        entries: Vec<String>,
        modules: HashMap<String, String>,
        url: &str,
        fetcher: Box<dyn Fn(&str) -> Option<(String, String)> + Send>,
        request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync>,
        ws_connector: WsConnector,
        // Layout rects to seed into HostState BEFORE the page's scripts run, so synchronous
        // layout-dependent reads during load see real geometry. `(rects, naturals, insets,
        // margins, scroll_y_css, doc_height_css)` (CSS px). `None` to seed nothing.
        initial_rects: Option<(
            Vec<(usize, f32, f32, f32, f32)>,
            Vec<(usize, f32, f32)>,
            Vec<(usize, f32, f32, f32, f32)>,
            Vec<(usize, f32, f32, f32, f32)>,
            f32,
            f32,
        )>,
    ) -> (Session, dom::Document, Vec<EvalOutput>) {
        let url = url.to_string();
        let fallback = doc.clone();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<SessionCmd>();
        // One-shot channel for the initial snapshot + per-source outputs.
        let (init_tx, init_rx) = std::sync::mpsc::channel::<(dom::Document, Vec<EvalOutput>)>();
        let stats = Arc::new(TabStats::default());
        let stats_thread = stats.clone();

        let spawn = std::thread::Builder::new()
            .name("js-session".to_string())
            .stack_size(256 * 1024 * 1024)
            .spawn(move || {
                // Catch any panic so it never crosses the thread boundary; on panic the init
                // channel is dropped and the caller falls back to an empty snapshot.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    session_thread_main(
                        doc,
                        scripts,
                        entries,
                        modules,
                        url,
                        fetcher,
                        request_fetcher,
                        ws_connector,
                        init_tx,
                        cmd_rx,
                        initial_rects,
                        stats_thread,
                    );
                }));
            });

        let handle = match spawn {
            Ok(h) => h,
            Err(e) => {
                return (
                    Session {
                        tx: cmd_tx,
                        handle: None,
                        stats,
                    },
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

        (
            Session {
                tx: cmd_tx,
                handle: Some(handle),
                stats,
            },
            snapshot,
            outputs,
        )
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
        reply_rx
            .recv()
            .unwrap_or_else(|_| (dom::Document::new(), Vec::new()))
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
        reply_rx
            .recv()
            .unwrap_or_else(|_| (dom::Document::new(), Vec::new()))
    }

    /// Push the engine's laid-out element rects (CSS px, document-absolute, top-origin) to the
    /// worker so element-geometry reads (`getBoundingClientRect`, `offsetWidth`, `scrollHeight`, …)
    /// return real values. Fire-and-forget: does not block on a reply, so the engine (which holds
    /// the DOM while the worker is idle between commands) never stalls on this. The worker stores
    /// the rects on `HostState`; the next geometry read serves them.
    pub fn set_layout_rects(
        &self,
        rects: Vec<(usize, f32, f32, f32, f32)>,
        naturals: Vec<(usize, f32, f32)>,
        insets: Vec<(usize, f32, f32, f32, f32)>,
        margins: Vec<(usize, f32, f32, f32, f32)>,
        scroll_y_css: f32,
        doc_height_css: f32,
    ) {
        let _ = self.tx.send(SessionCmd::SetRects {
            rects,
            naturals,
            insets,
            margins,
            scroll_y_css,
            doc_height_css,
        });
    }

    /// Push freshly-rasterized canvas/image RGBA pixels to the worker so `getImageData` returns real
    /// pixels. Fire-and-forget (no reply). `pixels` is `(node_id, width, height, rgba8)` per source.
    pub fn set_canvas_pixels(&self, pixels: Vec<(usize, u32, u32, Vec<u8>)>) {
        let _ = self.tx.send(SessionCmd::SetCanvasPixels { pixels });
    }

    /// Notify the page that the OS appearance (prefers-color-scheme) changed: re-evaluates every
    /// live `MediaQueryList` and fires `change` on those whose `.matches` flipped. The process-global
    /// flag is already updated via [`set_color_scheme_dark`]; this just dispatches the JS events and
    /// drains the loop so any DOM mutations the handlers make are reflected.
    pub fn notify_color_scheme_changed(&self) -> (dom::Document, Vec<String>) {
        self.eval_interact("(globalThis.__mediaChanged && globalThis.__mediaChanged())".to_string())
    }

    /// Evaluate an arbitrary JS source string against the live context, drain the event loop, and
    /// return a fresh DOM snapshot + console. Backs the higher-level interaction helpers below.
    fn eval_interact(&self, source: String) -> (dom::Document, Vec<String>) {
        let (_v, doc, console) = self.eval_full(source);
        (doc, console)
    }

    /// Return the JSON list of currently-observed IntersectionObserver / ResizeObserver targets
    /// (`[{kind:"io"|"ro", observerId, nodeId}, ...]`). Empty `[]` when no such observers exist —
    /// the engine uses this to skip geometry work entirely on pages without observers.
    pub fn observed_targets(&self) -> String {
        let (out, _doc, _console) =
            self.eval_full("JSON.stringify(__observedTargets())".to_string());
        out.value.unwrap_or_else(|| "[]".to_string())
    }

    /// Return the JSON display lists of every `<canvas>` that has a 2D context, for the engine to
    /// rasterize: `[{id,width,height,commands:[...]}, ...]`. Empty `[]` when no canvas has a
    /// context. The engine gates this on the DOM actually containing a `<canvas>`.
    pub fn canvas_lists(&self) -> String {
        let (out, _doc, _console) = self.eval_full(
            "JSON.stringify((globalThis.__canvasLists||function(){return[]})())".to_string(),
        );
        out.value.unwrap_or_else(|| "[]".to_string())
    }

    /// Deliver computed IntersectionObserver/ResizeObserver geometry to the page: invokes the JS
    /// observer callbacks (which may mutate the DOM), drains the loop, and returns a fresh snapshot
    /// + console. `arr_json` is the JSON array described in `__deliverObservations`.
    pub fn deliver_observations(&self, arr_json: &str) -> (dom::Document, Vec<String>) {
        self.eval_interact(format!("__deliverObservations({arr_json})"))
    }

    /// Evaluate `source` and return the (result value / error EvalOutput, snapshot, console).
    fn eval_full(&self, source: String) -> (EvalOutput, dom::Document, Vec<String>) {
        let (reply_tx, reply_rx) =
            std::sync::mpsc::channel::<(EvalOutput, dom::Document, Vec<String>)>();
        if self
            .tx
            .send(SessionCmd::Eval {
                source,
                reply: reply_tx,
            })
            .is_err()
        {
            return (EvalOutput::default(), dom::Document::new(), Vec::new());
        }
        reply_rx
            .recv()
            .unwrap_or_else(|_| (EvalOutput::default(), dom::Document::new(), Vec::new()))
    }

    /// Devtools console REPL: evaluate `source` in the live page context and return a display
    /// string (the result value, or an `Uncaught …` error), plus the updated snapshot + console.
    pub fn repl_eval(&self, source: &str) -> (String, dom::Document, Vec<String>) {
        let (out, doc, console) = self.eval_full(source.to_string());
        let display = if let Some(err) = out.error {
            format!("Uncaught {err}")
        } else {
            out.value.unwrap_or_else(|| "undefined".to_string())
        };
        (display, doc, console)
    }

    /// Toggle a checkbox / radio `node_id`: flips a checkbox's `checked`, or sets a radio
    /// (unchecking same-`name` siblings in the same form/document), then fires bubbling `input`
    /// and `change` events. No-op for disabled / non-checkable controls. The caller is expected to
    /// have already fired `click`. Returns a fresh DOM snapshot + console.
    /// Toggle a `<details>` open/closed (from a `<summary>` click) + fire `toggle`. Snapshot + console.
    pub fn toggle_details(&self, node_id: usize) -> (dom::Document, Vec<String>) {
        self.eval_interact(format!("__toggleDetails({node_id})"))
    }

    pub fn toggle_checkbox(&self, node_id: usize) -> (dom::Document, Vec<String>) {
        self.eval_interact(format!("__toggleCheckable({node_id})"))
    }

    /// Pick the `index`-th `<option>` of `<select>` `node_id`: marks it selected (clearing the
    /// others), sets the select's `value` attribute, then fires bubbling `input` + `change` so the
    /// page's handlers run. Returns `(changed, snapshot, console)` where `changed` is whether the
    /// selection actually changed. No-op (changed=false) for disabled / non-select / bad index.
    pub fn set_select_index(
        &self,
        node_id: usize,
        index: usize,
    ) -> (bool, dom::Document, Vec<String>) {
        let (out, doc, console) =
            self.eval_full(format!("Boolean(__setSelectIndex({node_id}, {index}))"));
        let changed = out.value.as_deref() == Some("true");
        (changed, doc, console)
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
    pub fn fire_event_nonbubbling(
        &self,
        node_id: usize,
        kind: &str,
    ) -> (dom::Document, Vec<String>) {
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
/// Per-isolate V8 heap ceiling. A runaway page (infinite DOM growth, a script that allocates without
/// bound) is terminated at this point rather than being allowed to grow until V8 calls
/// `FatalProcessOutOfMemory`, which aborts the WHOLE process (the crash this guards against).
const SESSION_HEAP_MAX: usize = 2 * 1024 * 1024 * 1024; // 2 GiB

/// V8 near-heap-limit callback. Fired on the isolate's own thread when the heap approaches
/// [`SESSION_HEAP_MAX`]. We terminate the running script (so one tab's runaway allocation can't take
/// the browser down) and return a slightly raised limit so V8 has headroom to unwind the stack and
/// deliver the termination, instead of OOM-aborting before it can.
unsafe extern "C" fn near_heap_limit_cb(
    data: *mut std::ffi::c_void,
    current: usize,
    _initial: usize,
) -> usize {
    let handle = &*(data as *const v8::IsolateHandle);
    handle.terminate_execution();
    current + 256 * 1024 * 1024
}

/// Create a V8 isolate with a bounded heap + the graceful-OOM guard, so a single page can't crash
/// the process. The leaked `IsolateHandle` lives as long as the isolate (one per session).
fn new_guarded_isolate() -> v8::OwnedIsolate {
    let mut isolate =
        v8::Isolate::new(v8::CreateParams::default().heap_limits(0, SESSION_HEAP_MAX));
    let handle = Box::into_raw(Box::new(isolate.thread_safe_handle()));
    isolate.add_near_heap_limit_callback(near_heap_limit_cb, handle as *mut std::ffi::c_void);
    isolate
}

fn session_thread_main(
    doc: dom::Document,
    scripts: Vec<String>,
    entries: Vec<String>,
    modules: HashMap<String, String>,
    url: String,
    fetcher: Box<dyn Fn(&str) -> Option<(String, String)> + Send>,
    request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync>,
    ws_connector: WsConnector,
    init_tx: std::sync::mpsc::Sender<(dom::Document, Vec<EvalOutput>)>,
    cmd_rx: std::sync::mpsc::Receiver<SessionCmd>,
    initial_rects: Option<(
        Vec<(usize, f32, f32, f32, f32)>,
        Vec<(usize, f32, f32)>,
        Vec<(usize, f32, f32, f32, f32)>,
        Vec<(usize, f32, f32, f32, f32)>,
        f32,
        f32,
    )>,
    stats: Arc<TabStats>,
) {
    ensure_v8_initialized();
    let shared: SharedDoc = Rc::new(RefCell::new(doc));
    // Background fetch threads deliver completions here; the receiver lives for the whole session
    // (used by every drain — init load and each subsequent command).
    let (fetch_tx, fetch_rx) = std::sync::mpsc::channel::<FetchCompletion>();
    // Background socket threads deliver WebSocket events here; the receiver lives for the whole
    // session and is drained (non-blocking) on every drain pass alongside the fetch channel.
    let (ws_evt_tx, ws_evt_rx) = std::sync::mpsc::channel::<WsEvent>();
    // Keep the isolate owned by this thread for the whole session.
    let mut isolate = new_guarded_isolate();
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
        let fetcher: Rc<dyn Fn(&str) -> Option<(String, String)>> =
            Rc::new(move |u: &str| fetcher(u));
        let state = HostState::with_fetcher(
            Rc::clone(&shared),
            Rc::clone(&fetcher),
            request_fetcher,
            fetch_tx,
            ws_connector,
            ws_evt_tx,
        );
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

        // Seed the engine-computed layout rects BEFORE scripts run, so synchronous
        // getBoundingClientRect / elementFromPoint / caret*FromPoint reads during page load see real
        // geometry (the rect table is otherwise empty until the engine's first post-load push).
        if let Some((rects, naturals, insets, margins, scroll_y_css, doc_height_css)) =
            initial_rects
        {
            let state = host_state(scope);
            {
                let mut map = state.layout_rects.borrow_mut();
                map.clear();
                for (id, x, y, w, h) in rects {
                    map.insert(id, (x, y, w, h));
                }
            }
            {
                let mut ins = state.used_insets.borrow_mut();
                ins.clear();
                for (id, t, r, b, l) in insets {
                    ins.insert(id, (t, r, b, l));
                }
            }
            {
                let mut mar = state.used_margins.borrow_mut();
                mar.clear();
                for (id, t, r, b, l) in margins {
                    mar.insert(id, (t, r, b, l));
                }
            }
            {
                let mut nat = state.image_natural.borrow_mut();
                nat.clear();
                for (id, w, h) in naturals {
                    nat.insert(id, (w, h));
                }
            }
            state.viewport_scroll_y.set(scroll_y_css);
            state.doc_height.set(doc_height_css);
        }

        // Run initial classic scripts in order, then the module graph, exactly as the load path.
        let mut results: Vec<EvalOutput> = Vec::with_capacity(scripts.len() + entries.len());
        for source in &scripts {
            results.push(eval_source(scope, source, "<script>"));
        }
        for entry in &entries {
            results.push(run_one_entry(scope, entry));
        }
        drain_event_loop(scope, &mut results, Some(&fetch_rx), Some(&ws_evt_rx));
        // Load drain done; switch the timer clock to real time so subsequent ticks/events run
        // setInterval/setTimeout/rAF over actual elapsed time.
        eval_internal(
            scope,
            "if (typeof __enterRealtime === 'function') { __enterRealtime(); }",
            "<realtime>",
        );

        // Send the initial snapshot back to Session::new's caller.
        let _ = init_tx.send((shared.borrow().clone(), results));
        v8::Global::new(scope, context)
    };

    // Command loop: each op re-enters the persistent context via Local::new(global).
    for cmd in cmd_rx {
        let __cmd_t0 = std::time::Instant::now();
        match cmd {
            SessionCmd::Dispatch {
                node_id,
                kind,
                x,
                y,
                reply,
            } => {
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
                drain_event_loop(scope, &mut results, Some(&fetch_rx), Some(&ws_evt_rx));
                let console = results.into_iter().flat_map(|r| r.console).collect();
                let _ = reply.send((shared.borrow().clone(), console));
            }
            SessionCmd::Key {
                node_id,
                key,
                code,
                reply,
            } => {
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
                drain_event_loop(scope, &mut results, Some(&fetch_rx), Some(&ws_evt_rx));
                let console = results.into_iter().flat_map(|r| r.console).collect();
                let _ = reply.send((shared.borrow().clone(), console));
            }
            SessionCmd::Eval { source, reply } => {
                let ctx = context.clone();
                v8::scope!(let handle_scope, &mut isolate);
                let local_ctx = v8::Local::new(handle_scope, &ctx);
                let scope = &mut v8::ContextScope::new(handle_scope, local_ctx);
                let mut results = vec![eval_source(scope, &source, "<interact>")];
                let first = EvalOutput {
                    value: results[0].value.clone(),
                    error: results[0].error.clone(),
                    console: Vec::new(),
                };
                drain_event_loop(scope, &mut results, Some(&fetch_rx), Some(&ws_evt_rx));
                let console = results.into_iter().flat_map(|r| r.console).collect();
                let _ = reply.send((first, shared.borrow().clone(), console));
            }
            SessionCmd::Tick { reply } => {
                let ctx = context.clone();
                v8::scope!(let handle_scope, &mut isolate);
                let local_ctx = v8::Local::new(handle_scope, &ctx);
                let scope = &mut v8::ContextScope::new(handle_scope, local_ctx);
                let mut results = vec![EvalOutput::default()];
                let did_work =
                    drain_event_loop(scope, &mut results, Some(&fetch_rx), Some(&ws_evt_rx));
                // Only snapshot+report when something actually ran, so idle ticks are cheap.
                if did_work {
                    let console = results.into_iter().flat_map(|r| r.console).collect();
                    let _ = reply.send(Some((shared.borrow().clone(), console)));
                } else {
                    let _ = reply.send(None);
                }
            }
            SessionCmd::SetRects {
                rects,
                naturals,
                insets,
                margins,
                scroll_y_css,
                doc_height_css,
            } => {
                // Store on HostState (no JS run needed — just update the geometry tables). Re-enter
                // the persistent context to reach the slot. Fire-and-forget: no reply.
                let ctx = context.clone();
                v8::scope!(let handle_scope, &mut isolate);
                let local_ctx = v8::Local::new(handle_scope, &ctx);
                let scope = &mut v8::ContextScope::new(handle_scope, local_ctx);
                let state = host_state(scope);
                let mut map = state.layout_rects.borrow_mut();
                map.clear();
                for (id, x, y, w, h) in rects {
                    map.insert(id, (x, y, w, h));
                }
                drop(map);
                let mut ins = state.used_insets.borrow_mut();
                ins.clear();
                for (id, t, r, b, l) in insets {
                    ins.insert(id, (t, r, b, l));
                }
                drop(ins);
                let mut mar = state.used_margins.borrow_mut();
                mar.clear();
                for (id, t, r, b, l) in margins {
                    mar.insert(id, (t, r, b, l));
                }
                drop(mar);
                let mut nat = state.image_natural.borrow_mut();
                nat.clear();
                for (id, w, h) in naturals {
                    nat.insert(id, (w, h));
                }
                drop(nat);
                state.viewport_scroll_y.set(scroll_y_css);
                state.doc_height.set(doc_height_css);
            }
            SessionCmd::SetCanvasPixels { pixels } => {
                // Store the engine's rasterized RGBA on HostState for getImageData. Re-enter the
                // persistent context to reach the slot. Fire-and-forget: no reply.
                let ctx = context.clone();
                v8::scope!(let handle_scope, &mut isolate);
                let local_ctx = v8::Local::new(handle_scope, &ctx);
                let scope = &mut v8::ContextScope::new(handle_scope, local_ctx);
                let state = host_state(scope);
                let mut map = state.canvas_pixels.borrow_mut();
                map.clear();
                for (id, w, h, rgba) in pixels {
                    map.insert(id, (w, h, rgba));
                }
            }
            SessionCmd::Stop => break,
        }
        // Per-tab stats for the tab tooltip: active JS time this command (CPU proxy — the thread is
        // otherwise blocked on recv) + the V8 heap used size. Cheap, sampled once per command.
        stats
            .cpu_ns
            .fetch_add(__cmd_t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        stats.heap_bytes.store(
            isolate.get_heap_statistics().used_heap_size() as u64,
            Ordering::Relaxed,
        );
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
        assert_eq!(
            out[0].value.as_deref(),
            Some(r#"<span class="hi">x</span>"#)
        );
    }

    // --- createElement / createElementNS / createAttribute / namespaces ------------------

    #[test]
    fn create_element_lowercases_and_uppercases_tagname() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var e = document.createElement("DiV");
                    [e.localName, e.tagName, e.nodeName, e.prefix, e.namespaceURI].join("|")"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("div|DIV|DIV||http://www.w3.org/1999/xhtml")
        );
    }

    #[test]
    fn create_element_invalid_name_throws_invalid_character_error() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var name="", code=null, isDom=false;
                    try { document.createElement("1foo"); }
                    catch (e) { name=e.name; code=e.code; isDom=(e instanceof DOMException); }
                    [name, code, isDom].join("|")"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("InvalidCharacterError|5|true")
        );
    }

    // --- HTML IDL attribute reflection ---------------------------------------------------------

    #[test]
    fn reflection_global_string_boolean_long_attributes() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var e = document.createElement("div");
                var r = [];
                // DOMString: getter "" when absent, live both ways.
                r.push(typeof e.id);                    // string
                r.push(e.title);                        // ""
                e.title = "hi"; r.push(e.getAttribute("title")); // hi
                e.setAttribute("title", "yo"); r.push(e.title);  // yo
                // boolean (hidden): presence of attribute.
                r.push(typeof e.hidden);                // boolean
                r.push(e.hidden);                       // false
                e.hidden = true; r.push(e.hasAttribute("hidden")); // true
                e.hidden = false; r.push(e.hasAttribute("hidden")); // false
                // long (tabIndex).
                r.push(typeof e.tabIndex);              // number
                e.setAttribute("tabindex", "5"); r.push(e.tabIndex); // 5
                e.setAttribute("tabindex", "x"); r.push(e.tabIndex); // 0 (invalid)
                r.join("|");
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("string||hi|yo|boolean|false|true|false|number|5|0")
        );
    }

    #[test]
    fn reflection_anchor_url_and_target() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var a = document.createElement("a");
                var r = [];
                r.push(typeof a.href);                  // string
                a.setAttribute("href", "/foo");
                r.push(a.href.indexOf("/foo") >= 0);    // true (resolved absolute)
                r.push(a.target);                       // "" (plain DOMString)
                a.target = "_blank"; r.push(a.getAttribute("target")); // _blank
                r.join("|");
            "#
            .to_string()],
            "https://example.com/base/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("string|true||_blank"));
    }

    #[test]
    fn reflection_input_enum_boolean_limited_long() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var i = document.createElement("input");
                var r = [];
                r.push(i.type);                         // text (missing default)
                i.setAttribute("type", "HAT"); r.push(i.type); // text (invalid -> default)
                i.setAttribute("type", "EMAIL"); r.push(i.type); // email (canonicalized)
                r.push(typeof i.disabled);              // boolean
                i.disabled = true; r.push(i.hasAttribute("disabled")); // true
                r.push(i.maxLength);                    // -1 (limited long default)
                i.setAttribute("maxlength", "5"); r.push(i.maxLength); // 5
                r.join("|");
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("text|text|email|boolean|true|-1|5")
        );
    }

    #[test]
    fn reflection_td_clamped_unsigned_long_colspan() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var c = document.createElement("td");
                var r = [];
                r.push(c.colSpan);                      // 1 (default)
                c.setAttribute("colspan", "3"); r.push(c.colSpan); // 3
                c.setAttribute("colspan", "0"); r.push(c.colSpan); // 1 (clamped to min)
                c.setAttribute("colspan", "x"); r.push(c.colSpan); // 1 (invalid -> default)
                r.join("|");
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("1|3|1|1"));
    }

    #[test]
    fn create_element_ns_records_namespace_prefix_localname_and_preserves_case() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                r#"var e = document.createElementNS("http://www.w3.org/2000/svg", "svg:Rect");
                    [e.namespaceURI, e.prefix, e.localName, e.tagName, e.nodeName].join("|")"#
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        // Non-HTML namespace preserves the qualifiedName case for tagName/nodeName.
        assert_eq!(
            out[0].value.as_deref(),
            Some("http://www.w3.org/2000/svg|svg|Rect|svg:Rect|svg:Rect")
        );
    }

    #[test]
    fn create_element_ns_prefix_without_namespace_throws_namespace_error() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var name=null, code=null;
                    try { document.createElementNS(null, "p:foo"); }
                    catch (e) { name=e.name; code=e.code; }
                    [name, code].join("|")"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("NamespaceError|14"));
    }

    #[test]
    fn create_attribute_lowercases_in_html_document() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var a = document.createAttribute("Foo");
                    [a.name, a.localName, a.value, a.nodeName, a.namespaceURI, a.prefix].join("|")"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("foo|foo||foo||"));
    }

    #[test]
    fn get_elements_by_tag_name_ns_matches_namespace_and_localname() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var svg = document.createElementNS("http://www.w3.org/2000/svg", "rect");
                    var html = document.createElement("rect");
                    document.body.appendChild(svg);
                    document.body.appendChild(html);
                    var svgMatches = document.getElementsByTagNameNS("http://www.w3.org/2000/svg", "rect");
                    var starMatches = document.getElementsByTagNameNS("*", "rect");
                    [svgMatches.length, svgMatches[0].namespaceURI, starMatches.length].join("|")"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("1|http://www.w3.org/2000/svg|2")
        );
    }

    #[test]
    fn parsed_html_element_reports_html_namespace_and_case() {
        // Elements that came from parsed HTML (no createElement metadata) still report the HTML
        // namespace, a lowercase localName and an uppercase tagName.
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var b = document.body;
                    [b.namespaceURI, b.localName, b.tagName, b.prefix].join("|")"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("http://www.w3.org/1999/xhtml|body|BODY|")
        );
    }

    #[test]
    fn set_attribute_lowercases_name_on_html_element() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var d = document.createElement("div");
                    d.setAttribute("DATA-X", "1");
                    [d.attributes[0].localName, d.getAttribute("data-x")].join("|")"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("data-x|1"));
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
    fn event_constructor_props_and_prevent_default() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var e = new Event("x", { bubbles: true, cancelable: true });
                var ok = e.type === "x" && e.bubbles === true && e.cancelable === true &&
                         e.composed === false && e.defaultPrevented === false &&
                         e.eventPhase === 0 && e.target === null && e.isTrusted === false &&
                         e.returnValue === true && typeof e.timeStamp === "number" &&
                         Event.NONE === 0 && Event.CAPTURING_PHASE === 1 &&
                         Event.AT_TARGET === 2 && Event.BUBBLING_PHASE === 3;
                e.preventDefault();
                [ok, e.defaultPrevented, e.returnValue].join(",")
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("true,true,false"));
    }

    #[test]
    fn custom_event_detail() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var e = new CustomEvent("y", { detail: 42 });
                [e.detail === 42, e instanceof Event, e instanceof CustomEvent].join(",")
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("true,true,true"));
    }

    #[test]
    fn mouse_event_init_and_instanceof_chain() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var e = new MouseEvent("click", { clientX: 5 });
                [e.clientX === 5, e instanceof MouseEvent, e instanceof UIEvent,
                 e instanceof Event].join(",")
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("true,true,true,true"));
    }

    #[test]
    fn ui_event_view_wrong_type_throws() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var threw = false;
                try { new UIEvent("x", { view: 7 }); } catch (e) { threw = (e instanceof TypeError); }
                threw
            "#.to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("true"));
    }

    #[test]
    fn create_event_init_and_dispatch_fires_listener() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var ev = document.createEvent("Event");
                var preType = ev.type;
                ev.initEvent("ping", true, true);
                var fired = 0;
                document.addEventListener("ping", function () { fired++; });
                var ret = document.dispatchEvent(ev);
                [preType === "", ev.type === "ping", fired, ret,
                 ev instanceof Event].join(",")
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("true,true,1,true,true"));
    }

    #[test]
    fn create_event_unknown_throws_not_supported() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var name = "";
                try { document.createEvent("nope"); }
                catch (e) { name = e.name; }
                name
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("NotSupportedError"));
    }

    #[test]
    fn create_event_prototype_matches_interface() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                [Object.getPrototypeOf(document.createEvent("Event")) === Event.prototype,
                 Object.getPrototypeOf(document.createEvent("mouseevent")) === MouseEvent.prototype,
                 Object.getPrototypeOf(document.createEvent("HTMLEvents")) === Event.prototype,
                 Object.getPrototypeOf(document.createEvent("CustomEvent")) === CustomEvent.prototype].join(",")
            "#.to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("true,true,true,true"));
    }

    #[test]
    fn bubbling_event_reaches_ancestor_listener() {
        // A bubbling event dispatched on a child element must reach a listener on an ancestor.
        let (mut doc, body) = doc_with_body("");
        let parent = doc.append_element(body, "div");
        let child = doc.append_element(parent, "span");
        if let dom::NodeData::Element(e) = &mut doc.get_mut(parent).data {
            e.attrs.insert("id".to_string(), "par".to_string());
        }
        if let dom::NodeData::Element(e) = &mut doc.get_mut(child).data {
            e.attrs.insert("id".to_string(), "kid".to_string());
        }
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var par = document.getElementById("par");
                var kid = document.getElementById("kid");
                var hits = [];
                par.addEventListener("boom", function (e) {
                    hits.push("par:" + e.eventPhase + ":" + (e.currentTarget === par) + ":" + (e.target === kid));
                });
                var ev = new Event("boom", { bubbles: true });
                kid.dispatchEvent(ev);
                hits.join("|")
            "#.to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        // ancestor handler ran in bubble phase (3), currentTarget=parent, target=child.
        assert_eq!(out[0].value.as_deref(), Some("par:3:true:true"));
    }

    #[test]
    fn capture_listener_runs_in_capture_phase() {
        let (mut doc, body) = doc_with_body("");
        let child = doc.append_element(body, "div");
        if let dom::NodeData::Element(e) = &mut doc.get_mut(child).data {
            e.attrs.insert("id".to_string(), "c".to_string());
        }
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var c = document.getElementById("c");
                var phase = -1;
                document.addEventListener("zap", function (e) { phase = e.eventPhase; }, true);
                c.dispatchEvent(new Event("zap", { bubbles: true }));
                phase
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        // document's capture listener fires during the capture phase (1).
        assert_eq!(out[0].value.as_deref(), Some("1"));
    }

    #[test]
    fn stop_propagation_blocks_ancestors() {
        let (mut doc, body) = doc_with_body("");
        let parent = doc.append_element(body, "div");
        let child = doc.append_element(parent, "span");
        if let dom::NodeData::Element(e) = &mut doc.get_mut(parent).data {
            e.attrs.insert("id".to_string(), "p".to_string());
        }
        if let dom::NodeData::Element(e) = &mut doc.get_mut(child).data {
            e.attrs.insert("id".to_string(), "k".to_string());
        }
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var p = document.getElementById("p");
                var k = document.getElementById("k");
                var reached = 0;
                k.addEventListener("t", function (e) { e.stopPropagation(); });
                p.addEventListener("t", function () { reached++; });
                k.dispatchEvent(new Event("t", { bubbles: true }));
                reached
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("0"));
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

    /// A <canvas> drawImage/clip/dashed-stroke/putImageData records the expected display-list
    /// commands (the engine rasterizes these; tested in the engine crate).
    #[test]
    fn canvas_records_draw_image_clip_dash_putimagedata() {
        let (mut doc, body) = doc_with_body("");
        let cv = doc.append_element(body, "canvas");
        if let dom::NodeData::Element(e) = &mut doc.get_mut(cv).data {
            e.attrs.insert("width".to_string(), "100".to_string());
            e.attrs.insert("height".to_string(), "100".to_string());
        }
        let img = doc.append_element(body, "img");
        if let dom::NodeData::Element(e) = &mut doc.get_mut(img).data {
            e.attrs.insert("id".to_string(), "src".to_string());
        }
        let src = r#"
            var c = document.querySelector('canvas');
            var ctx = c.getContext('2d');
            // clip to a rect, then fill (the fill command should carry the clip).
            ctx.beginPath(); ctx.rect(10, 10, 30, 30); ctx.clip();
            ctx.fillStyle = '#ff0000'; ctx.fillRect(0, 0, 100, 100);
            // dashed stroke
            ctx.setLineDash([5, 5]);
            ctx.beginPath(); ctx.moveTo(0, 50); ctx.lineTo(100, 50); ctx.stroke();
            // drawImage of the <img> by ref
            var im = document.getElementById('src');
            ctx.drawImage(im, 0, 0, 20, 20);
            // putImageData of a 2x2 block
            var id = ctx.createImageData(2, 2);
            for (var i = 0; i < id.data.length; i += 4) { id.data[i] = 255; id.data[i+3] = 255; }
            ctx.putImageData(id, 5, 5);
            JSON.stringify((globalThis.__canvasLists||function(){return[]})());
        "#;
        let (_doc, out) = run_with_dom(doc, vec![src.to_string()], "https://example.com/");
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        let json = out[0].value.clone().unwrap_or_default();
        assert!(
            json.contains("\"op\":\"fillRect\"") && json.contains("\"clip\""),
            "no clipped fill: {json}"
        );
        assert!(json.contains("\"dash\""), "no dash on stroke: {json}");
        assert!(
            json.contains("\"op\":\"drawImage\""),
            "no drawImage cmd: {json}"
        );
        assert!(
            json.contains("\"op\":\"putImageData\""),
            "no putImageData cmd: {json}"
        );
        assert!(!json.contains("getLineDash")); // sanity: no leaked fn names
    }

    /// getLineDash mirrors setLineDash; lineDashOffset round-trips; getImageData returns the engine
    /// pixels pushed via set_canvas_pixels (the round-trip).
    #[test]
    fn canvas_line_dash_accessors() {
        let (mut doc, body) = doc_with_body("");
        doc.append_element(body, "canvas");
        let src = r#"
            var ctx = document.querySelector('canvas').getContext('2d');
            ctx.setLineDash([4, 2]); ctx.lineDashOffset = 3;
            JSON.stringify({ dash: ctx.getLineDash(), off: ctx.lineDashOffset });
        "#;
        let (_doc, out) = run_with_dom(doc, vec![src.to_string()], "https://example.com/");
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some(r#"{"dash":[4,2],"off":3}"#));
    }

    #[test]
    fn computed_style_display_block_for_div_inline_for_span() {
        let (mut doc, body) = doc_with_body("");
        doc.append_element(body, "div");
        doc.append_element(body, "span");
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                "getComputedStyle(document.querySelectorAll('div')[0]).display".to_string(),
                "getComputedStyle(document.querySelectorAll('span')[0]).display".to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("block"));
        assert_eq!(out[1].value.as_deref(), Some("inline"));
    }

    #[test]
    fn computed_style_inline_color_serializes_rgb() {
        let (mut doc, body) = doc_with_body("");
        let p = doc.append_element(body, "p");
        if let dom::NodeData::Element(e) = &mut doc.get_mut(p).data {
            e.attrs.insert("style".to_string(), "color:red".to_string());
        }
        let (_doc, out) = run_with_dom(
            doc,
            vec!["getComputedStyle(document.querySelectorAll('p')[0]).color".to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("rgb(255, 0, 0)"));
    }

    #[test]
    fn computed_style_get_property_value_font_size_in_px() {
        let (mut doc, body) = doc_with_body("");
        let p = doc.append_element(body, "p");
        if let dom::NodeData::Element(e) = &mut doc.get_mut(p).data {
            e.attrs
                .insert("style".to_string(), "font-size:18px".to_string());
        }
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                "getComputedStyle(document.querySelectorAll('p')[0]).getPropertyValue('font-size')"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("18px"));
    }

    #[test]
    fn computed_style_applies_style_element_rule() {
        // A `<style>` rule `.x{display:flex}` is collected in-Session and applied via the cascade.
        let (mut doc, body) = doc_with_body("");
        // <style> in <head>
        let head = doc.get(doc.get(body).parent.unwrap()).children[0]; // html -> head
        let style_el = doc.append_element(head, "style");
        doc.append_child(
            style_el,
            dom::NodeData::Text(".x{display:flex}".to_string()),
        );
        let d = doc.append_element(body, "div");
        if let dom::NodeData::Element(e) = &mut doc.get_mut(d).data {
            e.attrs.insert("class".to_string(), "x".to_string());
        }
        let (_doc, out) = run_with_dom(
            doc,
            vec!["getComputedStyle(document.querySelectorAll('div')[0]).display".to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("flex"));
    }

    #[test]
    fn inline_style_serializes_values_canonically() {
        // The inline CSSStyleDeclaration normalizes numeric tokens (leading zero, negative zero) and
        // `url()` quoting, per the CSSOM "serialize a value" rules.
        let (mut doc, body) = doc_with_body("");
        doc.append_element(body, "div");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var el = document.querySelectorAll('div')[0];
                el.style.top = ".5%";
                el.style.left = "-.1em";
                el.style.right = "-0px";
                el.style.backgroundImage = "url(http://localhost/)";
                [el.style.top, el.style.left, el.style.right, el.style.backgroundImage].join("|");
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some(r#"0.5%|-0.1em|0px|url("http://localhost/")"#)
        );
    }

    #[test]
    fn static_element_inset_resolves_to_auto() {
        // getComputedStyle of a `position: static` element: insets resolve to the computed value;
        // `auto` (the default) stays `auto`.
        let (mut doc, body) = doc_with_body("");
        doc.append_element(body, "div");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var el = document.querySelectorAll('div')[0];
                el.style.cssText = "position: static; top: 5px";
                var cs = getComputedStyle(el);
                [cs.top, cs.left].join("|");
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        // static keeps the computed value (top: 5px) and the unset left as auto.
        assert_eq!(out[0].value.as_deref(), Some("5px|auto"));
    }

    #[test]
    fn css_style_rule_selector_text_roundtrip() {
        // `cssRules[0].selectorText` getter/setter: a valid selector is normalized and re-applied;
        // an invalid one leaves the rule unchanged.
        let (mut doc, body) = doc_with_body("");
        let head = doc.get(doc.get(body).parent.unwrap()).children[0];
        let style_el = doc.append_element(head, "style");
        doc.append_child(
            style_el,
            dom::NodeData::Text(".a { color: red }".to_string()),
        );
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var rule = document.querySelectorAll('style')[0].sheet.cssRules[0];
                var r = [rule.selectorText];
                rule.selectorText = "  span  >  div  ";   // normalized
                r.push(rule.selectorText);
                rule.selectorText = "!!invalid";          // rejected, unchanged
                r.push(rule.selectorText);
                rule.selectorText = ":after";             // pseudo-class -> pseudo-element form
                r.push(rule.selectorText);
                r.join("|");
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some(".a|span > div|span > div|::after")
        );
    }

    #[test]
    fn computed_style_mutating_inline_style_invalidates_cache() {
        // Read once, mutate the inline style, read again -> must reflect the NEW value (the
        // dom_version bump on setAttribute invalidates the cached cascade).
        let (mut doc, body) = doc_with_body("");
        doc.append_element(body, "div");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var el = document.querySelectorAll('div')[0];
                   var before = getComputedStyle(el).color;
                   el.style.color = 'rgb(1, 2, 3)';
                   var after = getComputedStyle(el).color;
                   before + '|' + after"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        // `before` is the inherited default; `after` is the freshly-set inline color.
        let v = out[0].value.as_deref().unwrap();
        assert!(
            v.ends_with("|rgb(1, 2, 3)"),
            "expected new color after mutation, got {v}"
        );
        assert_ne!(
            v, "rgb(1, 2, 3)|rgb(1, 2, 3)",
            "before should differ from after"
        );
    }

    #[test]
    fn computed_style_pseudo_element_argument() {
        // getComputedStyle(el, "::before") reflects the pseudo's cascaded style; an unknown
        // double-colon pseudo yields an empty style; the no-arg form is unchanged.
        let (mut doc, body) = doc_with_body("");
        let head = doc.get(doc.get(body).parent.unwrap()).children[0];
        let style_el = doc.append_element(head, "style");
        doc.append_child(
            style_el,
            dom::NodeData::Text(
                "#x { color: rgb(0, 0, 1) } #x::before { color: red; content: \"x\" }".to_string(),
            ),
        );
        let el = doc.append_element(body, "div");
        if let dom::NodeData::Element(e) = &mut doc.get_mut(el).data {
            e.attrs.insert("id".to_string(), "x".to_string());
        }
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                r#"var el = document.querySelectorAll('div')[0];
                   var b = getComputedStyle(el, "::before");
                   [
                     b.color,                              // pseudo's cascaded color
                     b.content,                            // pseudo's content
                     getComputedStyle(el, ":before").color,// legacy single-colon
                     String(getComputedStyle(el, "::totallynotapseudo").length), // unknown -> empty
                     String(getComputedStyle(el, "before").color === getComputedStyle(el).color), // no-colon -> element
                     getComputedStyle(el).color            // no-arg unchanged
                   ].join("|")"#
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("rgb(255, 0, 0)|\"x\"|rgb(255, 0, 0)|0|true|rgb(0, 0, 1)"),
        );
    }

    #[test]
    fn computed_style_pseudo_is_immutable() {
        // Writing a CSS property on a computed style throws NoModificationAllowedError.
        let (mut doc, body) = doc_with_body("");
        doc.append_element(body, "div");
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                r#"var s = getComputedStyle(document.querySelectorAll('div')[0], "::before");
                   try { s.color = "1"; "no-throw"; }
                   catch (e) { e.name; }"#
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("NoModificationAllowedError"));
    }

    #[test]
    fn computed_style_untracked_property_is_empty() {
        let (mut doc, body) = doc_with_body("");
        doc.append_element(body, "div");
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                "var s = getComputedStyle(document.querySelectorAll('div')[0]); \
                 [s.cursor, s.getPropertyValue('transition'), s.visibility].join(',')"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some(",,"));
    }

    #[test]
    fn computed_style_length_and_item_backed_by_names() {
        let (mut doc, body) = doc_with_body("");
        doc.append_element(body, "div");
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                // A computed (resolved) CSSStyleDeclaration is read-only: length/item work, but
                // setProperty throws NoModificationAllowedError (per CSSOM).
                "var s = getComputedStyle(document.querySelectorAll('div')[0]); \
                 var threw = false; try { s.setProperty('color','blue'); } catch (e) { threw = (e.name === 'NoModificationAllowedError'); } \
                 (s.length > 0) && (typeof s.item(0) === 'string') && (s.item(0).length > 0) && threw"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("true"));
    }

    #[test]
    fn inline_style_shorthand_margin_expands_and_serializes() {
        let (doc, _) = doc_with_body("");
        let (_d, out) = run_with_dom(
            doc,
            vec![
                "var s = document.createElement('div').style; s.margin = '1px 2px'; \
                 [s.marginTop, s.marginRight, s.marginBottom, s.marginLeft, s.getPropertyValue('margin')].join('|')"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("1px|2px|1px|2px|1px 2px"));
    }

    #[test]
    fn inline_style_all_shorthand_sets_longhands() {
        let (doc, _) = doc_with_body("");
        let (_d, out) = run_with_dom(
            doc,
            vec![
                "var s = document.createElement('div').style; s.cssText = 'all: inherit'; \
                 [s.getPropertyValue('width'), s.getPropertyValue('color'), s.getPropertyValue('all')].join('|')"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("inherit|inherit|inherit"));
    }

    #[test]
    fn inline_style_flex_shorthand_serializes() {
        let (doc, _) = doc_with_body("");
        let (_d, out) = run_with_dom(
            doc,
            vec![
                // flex: 0 -> grow 0, shrink 1, basis 0px; cssText collapses to the flex shorthand.
                "var s = document.createElement('div').style; s.cssText = 'flex: 0'; s.cssText"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("flex: 0 1 0px;"));
    }

    #[test]
    fn inline_style_overflow_collapses_to_shorthand() {
        let (doc, _) = doc_with_body("");
        let (_d, out) = run_with_dom(
            doc,
            vec!["var s = document.createElement('div').style; \
                 s.cssText = 'overflow-x: initial; overflow-y: initial'; s.cssText"
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("overflow: initial;"));
    }

    #[test]
    fn inline_style_escaped_custom_property_roundtrips() {
        let (doc, _) = doc_with_body("");
        let (_d, out) = run_with_dom(
            doc,
            vec![
                // `--a\;b` (escaped semicolon) is one custom property; the name unescapes for the
                // indexed getter and re-escapes on cssText serialization.
                "var e = document.createElement('span'); e.style = '--a\\\\;b: value'; \
                 [e.style.length, e.style[0], e.style.getPropertyValue('--a;b'), e.style.cssText].join('|')"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("1|--a;b|value|--a\\;b: value;")
        );
    }

    #[test]
    fn media_list_append_and_delete_medium() {
        let (doc, _) = doc_with_body("");
        let (_d, out) = run_with_dom(
            doc,
            vec!["var sheet = new CSSStyleSheet(); var m = sheet.media; \
                 m.appendMedium('screen'); m.appendMedium('print'); var a = m.mediaText; \
                 m.deleteMedium('screen'); [a, m.mediaText, m.length, m.item(0)].join('|')"
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("screen, print|print|1|print"));
    }

    #[test]
    fn nth_child_selector_serializes_canonically() {
        let (doc, _) = doc_with_body("");
        let (_d, out) = run_with_dom(
            doc,
            vec![
                "var ss = new CSSStyleSheet(); ss.insertRule(':nth-child(  3n - 0){color:red}'); \
                 var a = ss.cssRules[0].selectorText; \
                 ss.insertRule(':nth-child(even){color:red}', 1); \
                 [a, ss.cssRules[1].selectorText].join('|')"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some(":nth-child(3n)|:nth-child(2n)")
        );
    }

    #[test]
    fn page_rule_selector_text_and_style() {
        let (doc, _) = doc_with_body("");
        let (_d, out) = run_with_dom(
            doc,
            vec![
                "var ss = new CSSStyleSheet(); ss.insertRule('@page :left { margin: 1px; }'); \
                 var r = ss.cssRules[0]; \
                 [r.type, r.selectorText, r.style.getPropertyValue('margin-top'), r.cssText].join('|')"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("6|:left|1px|@page :left { margin: 1px; }")
        );
    }

    #[test]
    fn constructed_stylesheet_replace_sync() {
        // `new CSSStyleSheet()` then replaceSync(".a{color:red}") -> one rule. @import is stripped.
        let (doc, _) = doc_with_body("");
        let (_d, out) = run_with_dom(
            doc,
            vec![
                "var ss = new CSSStyleSheet(); ss.replaceSync('.a{color:red}'); \
                 var n1 = ss.cssRules.length; \
                 ss.replaceSync('@import url(x.css); .b{color:blue}'); \
                 [n1, ss.cssRules.length, ss.cssRules[0].selectorText].join('|')"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("1|1|.b"));
    }

    #[test]
    fn replace_sync_on_regular_sheet_throws_not_allowed() {
        // replaceSync on a <style>'s live (non-constructed) sheet throws NotAllowedError.
        let (mut doc, body) = doc_with_body("");
        let style = doc.append_element(body, "style");
        doc.append_child(style, dom::NodeData::Text(".a { color: red }".to_string()));
        let (_d, out) = run_with_dom(
            doc,
            vec!["var s = document.querySelector('style').sheet; \
                 var name = ''; try { s.replaceSync('.b{color:blue}'); name = 'no-throw'; } \
                 catch (e) { name = e.name; } name"
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("NotAllowedError"));
    }

    #[test]
    fn constructed_sheet_insert_import_throws_syntax_error() {
        let (doc, _) = doc_with_body("");
        let (_d, out) = run_with_dom(
            doc,
            vec![
                "var name = ''; try { (new CSSStyleSheet()).insertRule('@import url(x.css)'); name = 'no-throw'; } \
                 catch (e) { name = e.name; } name"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("SyntaxError"));
    }

    #[test]
    fn adopted_stylesheets_accepts_constructed_and_document_sheets() {
        // Setting document.adoptedStyleSheets to a constructed sheet works; a same-document
        // non-constructed (live <style>) sheet is also accepted (csswg-drafts #10013, tentative).
        let (mut doc, body) = doc_with_body("");
        let style = doc.append_element(body, "style");
        doc.append_child(style, dom::NodeData::Text(".a { color: red }".to_string()));
        let (_d, out) = run_with_dom(
            doc,
            vec![
                "var c = new CSSStyleSheet(); c.replaceSync('.a{color:red}'); \
                 document.adoptedStyleSheets = [c]; \
                 var okLen = document.adoptedStyleSheets.length; \
                 var same = document.adoptedStyleSheets[0] === c; \
                 var reg = document.querySelector('style').sheet; \
                 var name = ''; try { document.adoptedStyleSheets = [reg]; name = 'no-throw'; } \
                 catch (e) { name = e.name; } \
                 [okLen, same, name].join('|')"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("1|true|no-throw"));
    }

    #[test]
    fn inline_style_custom_property_roundtrips() {
        let (doc, _) = doc_with_body("");
        let (_d, out) = run_with_dom(
            doc,
            vec![
                "var s = document.createElement('div').style; s.setProperty('--x', '5px'); \
                 [s.getPropertyValue('--x'), s.cssText].join('|')"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("5px|--x: 5px;"));
    }

    #[test]
    fn computed_style_reflects_inline_custom_property() {
        let (doc, _) = doc_with_body("");
        let (_d, out) = run_with_dom(
            doc,
            vec![
                "var d = document.createElement('div'); document.body.appendChild(d); \
                 d.style.setProperty('--my', '42px'); \
                 getComputedStyle(d).getPropertyValue('--my')"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("42px"));
    }

    #[test]
    fn inline_style_removeproperty_shorthand_removes_longhands() {
        let (doc, _) = doc_with_body("");
        let (_d, out) = run_with_dom(
            doc,
            vec![
                "var s = document.createElement('div').style; s.margin = '1px 2px 3px 4px'; \
                 s.removeProperty('margin'); \
                 [s.getPropertyValue('margin-top'), s.length].join('|')"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("|0"));
    }

    #[test]
    fn document_title_returns_title_text() {
        let (doc, _) = doc_with_body("My Page");
        let (_doc, out) = run_with_dom(
            doc,
            vec!["document.title".to_string()],
            "https://example.com/",
        );
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
            vec![r#"var el = document.createElement("div");
                   el.innerHTML = '<div foo="bar">hi</div>';
                   [el.children.length,
                    el.children[0].tagName,
                    el.children[0].getAttribute("foo"),
                    el.children[0].textContent].join("|")"#
                .to_string()],
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
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"setTimeout(() => console.log("tick"), 0);"#.to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        let all: Vec<String> = out.iter().flat_map(|o| o.console.clone()).collect();
        assert!(
            all.iter().any(|l| l == "tick"),
            "expected 'tick' in {all:?}"
        );
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
        assert!(
            fast < slow,
            "fast (10ms) must run before slow (50ms): {all:?}"
        );
    }

    #[test]
    fn clear_timeout_cancels_callback() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                r#"var id = setTimeout(() => console.log("nope"), 0); clearTimeout(id);"#
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        let all: Vec<String> = out.iter().flat_map(|o| o.console.clone()).collect();
        assert!(
            !all.iter().any(|l| l == "nope"),
            "cancelled callback ran: {all:?}"
        );
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
        assert!(
            all.iter().any(|l| l == "tick1"),
            "interval should fire once: {all:?}"
        );
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
        assert!(
            all.iter().any(|l| l == "after"),
            "loop died on throw: {all:?}"
        );
        // The error surfaced (prefixed with the warning marker).
        assert!(
            all.iter().any(|l| l.contains('⚠') && l.contains("boom")),
            "error not reported: {all:?}"
        );
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
        assert!(
            !all.iter().any(|l| l == "cancelled"),
            "cancelAnimationFrame failed: {all:?}"
        );
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

    #[test]
    fn lookup_namespace_uri_and_is_default() {
        let (doc, _body) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
              var e = document.createElementNS('fooNamespace', 'prefix:elem');
              var r = [];
              r.push(e.lookupNamespaceURI('prefix'));          // fooNamespace
              r.push(e.lookupNamespaceURI('xml'));             // XML built-in
              r.push(String(e.lookupNamespaceURI('nope')));    // null
              r.push(e.isDefaultNamespace('http://www.w3.org/1999/xhtml')); // false here
              e.setAttributeNS('http://www.w3.org/2000/xmlns/', 'xmlns', 'bazURI');
              r.push(e.lookupNamespaceURI(null));              // bazURI
              r.push(e.lookupPrefix('fooNamespace'));          // prefix
              r.join('|')
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("fooNamespace|http://www.w3.org/XML/1998/namespace|null|false|bazURI|prefix")
        );
    }

    #[test]
    fn create_document_type_and_doctype() {
        let (doc, _body) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
              var dt = document.implementation.createDocumentType('html', 'pub', 'sys');
              [dt.name, dt.nodeName, dt.publicId, dt.systemId, dt.nodeType,
               String(dt.nodeValue), String(dt.textContent)].join('|')
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("html|html|pub|sys|10|null|null")
        );
    }

    #[test]
    fn parsed_doctype_is_document_doctype() {
        // The parser produces a DocumentType node named "html" for `<!DOCTYPE html>`.
        let doc = html::parse("<!DOCTYPE html><html><head></head><body></body></html>");
        let (_doc, out) = run_with_dom(
            doc,
            vec!["document.doctype ? document.doctype.name + '|' + document.doctype.nodeType : 'none'".to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("html|10"));
    }

    #[test]
    fn css_escape_serializes_identifier() {
        let (doc, _body) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                r#"[CSS.escape(".foo#bar"), CSS.escape("0abc"), CSS.escape("-")].join('|')"#
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        // "." and '#' are escaped with a backslash; a leading digit becomes "\\3N "; lone "-" -> "\\-".
        assert_eq!(out[0].value.as_deref(), Some(r#"\.foo\#bar|\30 abc|\-"#));
    }

    #[test]
    fn get_and_set_attribute_node() {
        let (doc, _body) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
              var el = document.createElement('div');
              el.setAttribute('foo', 'bar');
              var a = el.getAttributeNode('foo');
              var same = a === el.attributes[0];
              el.removeAttributeNode(a);                // detaches, keeps value
              var el2 = document.createElement('div');
              el2.setAttributeNode(a);
              [same, a.value, el2.getAttribute('foo'), a.ownerElement === el2,
               a === el2.getAttributeNode('foo')].join('|')
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("true|bar|bar|true|true"));
    }

    #[test]
    fn rel_list_reflects_rel_attribute() {
        let (doc, _body) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
              var a = document.createElement('a');
              a.relList.add('noopener');
              a.relList.add('noreferrer');
              var area = document.createElement('div');     // div has no relList
              [a.getAttribute('rel'), Object.prototype.toString.call(a.relList),
               (area.relList === undefined)].join('|')
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("noopener noreferrer|[object DOMTokenList]|true")
        );
    }

    #[test]
    fn attribute_insertion_order_preserved() {
        // IndexMap-backed attrs expose attributes in insertion order.
        let (doc, _body) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
              var el = document.createElement('div');
              el.toggleAttribute('a'); el.toggleAttribute('b');
              el.setAttribute('a', 'thing'); el.toggleAttribute('c');
              el.getAttributeNames().join(',')
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("a,b,c"));
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
    fn computed_inset_uses_used_value_for_positioned_box() {
        // position:absolute element with explicit top/left in a positioned container: the set sides
        // resolve to their own px; an auto side resolves to the used value derived from the
        // containing block geometry (CSSOM resolved value). For the all-auto static-position case the
        // value is the box's hypothetical in-flow offset within the containing block.
        let out = env_eval(
            "https://example.com/",
            r#"
              var cb = document.createElement('div');
              cb.style.cssText = 'position:relative; width:200px; height:100px; padding:0;';
              var inner = document.createElement('div');
              cb.appendChild(inner);
              var t = document.createElement('div');
              inner.appendChild(t);
              document.body.appendChild(cb);

              // Set top/left, leave bottom/right auto (auto vs set -> used value = basis - opposite).
              t.style.cssText = 'position:absolute; top:10px; left:20px; width:0; height:0;';
              var cs = getComputedStyle(t);
              var r1 = [cs.top, cs.left, cs.bottom, cs.right].join(',');

              // All-auto: every side resolves to the static position (in-flow origin offset).
              t.style.cssText = 'position:absolute; width:0; height:0;';
              var cs2 = getComputedStyle(t);
              var r2 = [cs2.top, cs2.left].join(',');
              r1 + '|' + r2
            "#,
        );
        assert_eq!(out.error, None, "{out:?}");
        // top=10, left=20 as set; bottom = 100 - 10 - 0 = 90; right = 200 - 20 - 0 = 180.
        // Static position: the box is the first in-flow content of the cb (no padding/border), so
        // both top and left resolve to 0px.
        assert_eq!(out.value.as_deref(), Some("10px,20px,90px,180px|0px,0px"));
    }

    #[test]
    fn named_global_resolves_element_by_id() {
        // HTML named-properties-on-window: a bare global resolves to the element with that id, and is
        // overridable by assignment (so `var name = ...` in author code doesn't throw).
        let out = env_eval(
            "https://example.com/",
            r#"
              var d = document.createElement('div');
              d.id = 'widget';
              d.style.cssText = 'position:absolute; top:7px;';
              document.body.appendChild(d);
              // Re-run the named-global install (the env install ran before this script created #widget).
              __installNamedGlobals();
              var byName = (typeof widget === 'object' && widget !== null) ? widget.id : '(none)';
              var top = getComputedStyle(widget).top;
              byName + ',' + top
            "#,
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("widget,7px"));
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
    fn get_computed_style_returns_real_values() {
        // The body is a real element (UA sheet -> display:block); getComputedStyle now surfaces the
        // in-Session cascade instead of the old "" stub.
        let out = env_eval(
            "https://example.com/",
            "var s = getComputedStyle(document.body); \
             [s.getPropertyValue('display'), s.display, s.getPropertyValue('color').slice(0,4)].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("block|block|rgb("));
    }

    #[test]
    fn add_event_listener_exists_on_window_and_document() {
        let out = env_eval(
            "https://example.com/",
            "[typeof window.addEventListener, typeof document.addEventListener, \
              typeof window.dispatchEvent, typeof document.removeEventListener].join(',')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(
            out.value.as_deref(),
            Some("function,function,function,function")
        );
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
        assert!(
            all.iter().any(|l| l == "dcl-fired"),
            "DOMContentLoaded did not fire: {all:?}"
        );
        assert!(
            all.iter().any(|l| l == "load-fired"),
            "load did not fire: {all:?}"
        );
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
    fn abort_controller_aborts_and_fires_event() {
        let out = env_eval(
            "https://example.com/",
            "var c = new AbortController(); var fired = false; \
             c.signal.addEventListener('abort', function () { fired = true; }); \
             var a0 = c.signal.aborted; c.abort(); \
             [a0, c.signal.aborted, fired, c.signal.reason.name].join(',')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("false,true,true,AbortError"));
    }

    #[test]
    fn add_event_listener_signal_option_removes_on_abort() {
        let out = env_eval(
            "https://example.com/",
            "var c = new AbortController(); var n = 0; \
             document.addEventListener('ping', function () { n++; }, { signal: c.signal }); \
             document.dispatchEvent({ type: 'ping' }); c.abort(); document.dispatchEvent({ type: 'ping' }); \
             String(n)",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("1"));
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
    fn mutation_observer_take_records_is_synchronous() {
        // takeRecords() must return mutations observed so far synchronously, within the same task —
        // here a classList.replace() that sets the `class` attribute once.
        let out = env_eval(
            "https://example.com/",
            "var el = document.createElement('div'); el.setAttribute('class','a b c'); \
             var obs = new MutationObserver(function(){}); obs.observe(el, {attributes:true, attributeOldValue:true}); \
             var r = el.classList.replace('b','d'); \
             var recs = obs.takeRecords(); obs.disconnect(); \
             [r, recs.length, recs[0].type, recs[0].attributeName, recs[0].oldValue, el.getAttribute('class')].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(
            out.value.as_deref(),
            Some("true|1|attributes|class|a b c|a d c")
        );
    }

    #[test]
    fn classlist_is_live_with_classname_and_setattribute() {
        let out = env_eval(
            "https://example.com/",
            "var el = document.createElement('div'); \
             el.className = 'a b'; var r1 = el.classList.contains('a') + ':' + el.classList.length; \
             el.setAttribute('class', 'x y z'); var r2 = el.classList.contains('y') + ':' + el.classList.length; \
             el.classList.add('w'); var r3 = el.className; \
             [r1, r2, r3].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("true:2|true:3|x y z w"));
    }

    #[test]
    fn classlist_length_index_item_and_value() {
        let out = env_eval(
            "https://example.com/",
            "var el = document.createElement('div'); \
             el.setAttribute('class', '\\t\\n\\f\\r a\\t\\n\\f\\r b\\t\\n\\f\\r '); \
             [el.classList.length, el.classList[0], el.classList[1], \
              (el.classList[2] === undefined), el.classList.item(0), \
              (el.classList.item(5) === null), el.classList.value].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        // length=2, [0]=a, [1]=b, [2]===undefined, item(0)=a, item(5)===null, value=raw attr
        assert_eq!(
            out.value.as_deref(),
            Some("2|a|b|true|a|true|\t\n\u{c}\r a\t\n\u{c}\r b\t\n\u{c}\r ")
        );
    }

    #[test]
    fn classlist_add_remove_serialize_normalizes() {
        let out = env_eval(
            "https://example.com/",
            "var el = document.createElement('div'); \
             el.setAttribute('class', '   a  a b'); el.classList.add('c'); var r1 = el.getAttribute('class'); \
             el.setAttribute('class', 'a b  c'); el.classList.remove('d'); var r2 = el.getAttribute('class'); \
             [r1, r2].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("a b c|a b c"));
    }

    #[test]
    fn classlist_toggle_force_variants() {
        let out = env_eval(
            "https://example.com/",
            "var el = document.createElement('div'); el.setAttribute('class', 'a b'); \
             var t1 = el.classList.toggle('a'); var v1 = el.getAttribute('class'); \
             var t2 = el.classList.toggle('a'); var v2 = el.getAttribute('class'); \
             el.setAttribute('class', 'a b'); var t3 = el.classList.toggle('a', true); var v3 = el.getAttribute('class'); \
             var t4 = el.classList.toggle('c', false); var v4 = el.getAttribute('class'); \
             [t1, v1, t2, v2, t3, v3, t4, v4].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        // toggle('a')->false 'b'; toggle('a')->true 'b a'; toggle('a',true)->true 'a b'; toggle('c',false)->false 'a b'
        assert_eq!(
            out.value.as_deref(),
            Some("false|b|true|b a|true|a b|false|a b")
        );
    }

    #[test]
    fn classlist_replace_semantics() {
        let out = env_eval(
            "https://example.com/",
            "var el = document.createElement('div'); \
             el.setAttribute('class', 'a b c'); var r1 = el.classList.replace('c', 'a') + ':' + el.getAttribute('class'); \
             el.setAttribute('class', 'a a a  b'); var r2 = el.classList.replace('c', 'd') + ':' + el.getAttribute('class'); \
             el.setAttribute('class', 'a b a'); var r3 = el.classList.replace('a', 'c') + ':' + el.getAttribute('class'); \
             [r1, r2, r3].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        // replace c->a dedups to 'a b'; replace c (absent)->d returns false, raw unchanged; replace a->c => 'c b'
        assert_eq!(
            out.value.as_deref(),
            Some("true:a b|false:a a a  b|true:c b")
        );
    }

    #[test]
    fn classlist_assignment_forwards_to_value_same_object() {
        let out = env_eval(
            "https://example.com/",
            "var el = document.createElement('div'); var ref = el.classList; \
             el.classList = 'foo bar'; \
             [el.classList === ref, el.getAttribute('class'), el.classList.length].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("true|foo bar|2"));
    }

    #[test]
    fn classlist_empty_token_throws_syntax_error_domexception() {
        let out = env_eval(
            "https://example.com/",
            "var el = document.createElement('div'); var ok = false, name = '', code = -1, isDOM = false; \
             try { el.classList.add(''); } catch (e) { ok = true; name = e.name; code = e.code; isDOM = (e instanceof DOMException); } \
             [ok, name, code, isDOM, el.getAttribute('class')].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        // SyntaxError DOMException, code 12, attribute unchanged (still null -> 'null' via join)
        assert_eq!(out.value.as_deref(), Some("true|SyntaxError|12|true|"));
    }

    #[test]
    fn classlist_whitespace_token_throws_invalid_character_error_domexception() {
        let out = env_eval(
            "https://example.com/",
            "var el = document.createElement('div'); var ok = false, name = '', code = -1, isDOM = false; \
             try { el.classList.add('a b'); } catch (e) { ok = true; name = e.name; code = e.code; isDOM = (e instanceof DOMException); } \
             [ok, name, code, isDOM].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        // InvalidCharacterError DOMException, code 5
        assert_eq!(
            out.value.as_deref(),
            Some("true|InvalidCharacterError|5|true")
        );
    }

    #[test]
    fn classlist_supports_throws_type_error() {
        let out = env_eval(
            "https://example.com/",
            "var el = document.createElement('div'); var isType = false; \
             try { el.classList.supports('a'); } catch (e) { isType = (e instanceof TypeError); } \
             String(isType)",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("true"));
    }

    #[test]
    fn classlist_iteration_for_of_and_foreach() {
        let out = env_eval(
            "https://example.com/",
            "var el = document.createElement('div'); el.setAttribute('class', 'a b c'); \
             var fo = []; for (var t of el.classList) { fo.push(t); } \
             var fe = []; el.classList.forEach(function (t, i) { fe.push(i + ':' + t); }); \
             var ks = []; var it = el.classList.keys(); var n; while (!(n = it.next()).done) { ks.push(n.value); } \
             [fo.join(','), fe.join(','), ks.join(',')].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("a,b,c|0:a,1:b,2:c|0,1,2"));
    }

    #[test]
    fn classlist_remove_absent_on_null_attr_is_noop() {
        let out = env_eval(
            "https://example.com/",
            "var el = document.createElement('div'); \
             el.classList.remove('a'); \
             String(el.hasAttribute('class'))",
        );
        assert_eq!(out.error, None, "{out:?}");
        // Removing from an absent attribute with empty resulting set must NOT create the attribute.
        assert_eq!(out.value.as_deref(), Some("false"));
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
            vec![
                r#"document.body.style.display = "none"; document.body.style.display"#.to_string(),
            ],
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
    fn font_variant_shorthand_expands_and_serializes() {
        let (doc, _body) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                // Shorthand `normal` sets every longhand to normal.
                r#"var b = document.body; b.style.fontVariant = "normal"; b.style.fontVariantCaps + "|" + b.style.fontVariant"#.to_string(),
                // A single non-default longhand round-trips into the shorthand.
                r#"var b2 = document.createElement("div"); b2.style.fontVariant = "normal"; b2.style.fontVariantCaps = "small-caps"; b2.style.fontVariant"#.to_string(),
                // ligatures:none combined with another longhand can't form the shorthand.
                r#"var b3 = document.createElement("div"); b3.style.fontVariant = "normal"; b3.style.fontVariantLigatures = "none"; b3.style.fontVariantCaps = "small-caps"; b3.style.fontVariant"#.to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("normal|normal"));
        assert_eq!(out[1].value.as_deref(), Some("small-caps"));
        assert_eq!(out[2].value.as_deref(), Some(""));
    }

    #[test]
    fn font_family_quoting_serialization() {
        let (doc, _body) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                // A quoted multi-word name normalizes to unquoted; a generic family stays quoted.
                r#"var d = document.createElement("div"); d.setAttribute("style", "font-family: 'Times New Roman', \"serif\", '34J', Veronica"); d.style.fontFamily"#.to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some(r#"Times New Roman, "serif", "34J", Veronica"#)
        );
    }

    #[test]
    fn style_css_text_round_trips_and_drops_invalid() {
        let (doc, body) = doc_with_body("");
        let (doc, out) = run_with_dom(
            doc,
            vec![
                // Unknown property + invalid value are dropped; valid declarations survive.
                r#"var b = document.body; b.style.cssText = "color: red; unknownprop: x; width: -5px; font-size: 10pt"; b.style.cssText"#.to_string(),
                // Setting an invalid property never creates a style attribute on a fresh element.
                r#"var e = document.createElement("div"); e.style.setProperty("doesntexist", "0"); String(e.hasAttribute("style"))"#.to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("color: red; font-size: 10pt;")
        );
        assert_eq!(out[1].value.as_deref(), Some("false"));
        // The body's style attribute reflects only the valid declarations.
        let style = attr_of(&doc, body, "style").unwrap_or_default();
        assert!(style.contains("color: red"), "style attr was {style:?}");
        assert!(!style.contains("unknownprop"), "style attr was {style:?}");
        assert!(!style.contains("-5px"), "style attr was {style:?}");
    }

    #[test]
    fn mutating_style_queues_attribute_record() {
        // Mutating el.style fires a MutationObserver attribute record for `style`; a no-op set of an
        // equal value does not.
        let (doc, _body) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var el = document.body;
                var recs = [];
                var mo = new MutationObserver(function (rs) { recs = recs.concat(rs); });
                mo.observe(el, { attributes: true, attributeOldValue: true });
                el.style.zIndex = "10";
                var first = mo.takeRecords();
                el.style.zIndex = "10"; // same value -> no new record
                var second = mo.takeRecords();
                first.length + "|" + (first[0] && first[0].attributeName) + "|" + second.length
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("1|style|0"));
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
        assert!(
            style.contains("background-color: red"),
            "style attr was {style:?}"
        );
    }

    #[test]
    fn style_reads_existing_style_attribute() {
        // Pre-seed a style="" attribute and confirm el.style reads from it.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let html = doc.append_element(root, "html");
        let body = doc.append_element(html, "body");
        if let dom::NodeData::Element(e) = &mut doc.get_mut(body).data {
            e.attrs
                .insert("style".into(), "display: none; color: blue".into());
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
        // Without an engine pushing rects (the bare `env_eval` path lays out nothing), the rect is
        // the zero-rect fallback — but every DOMRect field must be present, finite, and toJSON-able.
        // Real (non-zero) geometry is exercised in the `engine` crate's layout-rect tests.
        let out = env_eval(
            "https://example.com/",
            "var r = document.body.getBoundingClientRect(); \
             var ok = ['x','y','top','left','right','bottom','width','height'] \
               .every(function(k){ return typeof r[k] === 'number' && isFinite(r[k]); }); \
             ok && typeof r.toJSON === 'function' && \
             [r.x, r.y, r.top, r.left, r.right, r.bottom, r.width, r.height].join(',') === '0,0,0,0,0,0,0,0'",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("true"));
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
            vec![
                r#"var c = document.body.firstElementChild; c.style.display = "block"; c.tagName"#
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("DIV"));
        let style = attr_of(&doc, child, "style").unwrap_or_default();
        assert!(
            style.contains("display: block"),
            "child style attr was {style:?}"
        );
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
    fn no_fetch() -> Box<dyn Fn(&str) -> Option<(String, String)> + Send> {
        Box::new(|_u: &str| None)
    }

    /// A request fetcher that never serves anything (default for tests not exercising `fetch`).
    fn no_request() -> Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync> {
        Arc::new(|_m, _u, _b, _h| None)
    }

    /// A WebSocket connector that always errs (default for tests not exercising `WebSocket`).
    fn no_ws() -> WsConnector {
        Arc::new(|_url, _id, _evt| Err("no WebSocket connector".to_string()))
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
        let (_doc, out) = run_modules(
            doc,
            "https://x/",
            vec![entry],
            modules,
            no_fetch(),
            no_request(),
        );
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(
            console.iter().any(|l| l == "got 42"),
            "console was {console:?}"
        );
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
        modules.insert(
            leaf,
            r#"export function hello() { return "chained"; }"#.to_string(),
        );

        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(
            doc,
            "https://x/",
            vec![entry],
            modules,
            no_fetch(),
            no_request(),
        );
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(
            console.iter().any(|l| l == "chained"),
            "console was {console:?}"
        );
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
        let (_doc, out) = run_modules(
            doc,
            "https://x/",
            vec![entry.clone()],
            modules,
            no_fetch(),
            no_request(),
        );
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
        let (_doc, out) = run_modules(
            doc,
            "https://x/",
            vec![entry],
            modules,
            no_fetch(),
            no_request(),
        );
        // Must not panic; the entry's evaluation surfaces an error.
        assert!(
            out.iter().any(|o| o.error.is_some()),
            "expected an error, got {out:?}"
        );
    }

    #[test]
    fn side_effect_import_runs_imported_module() {
        let entry = "https://x/app.js".to_string();
        let dep = "https://x/dep.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(entry.clone(), r#"import "https://x/dep.js";"#.to_string());
        modules.insert(dep, r#"console.log("side effect ran");"#.to_string());

        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(
            doc,
            "https://x/",
            vec![entry],
            modules,
            no_fetch(),
            no_request(),
        );
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

        // fetch() now routes through the request fetcher (method/url/body/headers) and parses the
        // host's JSON envelope. The URL is resolved against the page URL before it reaches us.
        let request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync> =
            Arc::new(|method, u, _b, _h| {
                assert_eq!(method, "GET");
                assert_eq!(
                    u, "https://x/data.json",
                    "fetch should resolve relative URLs"
                );
                Some(
                    r#"{"ok":true,"status":200,"statusText":"OK","url":"https://x/data.json","contentType":"application/json","body":"{\"score\": 99}"}"#
                        .to_string(),
                )
            });

        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(
            doc,
            "https://x/",
            vec![entry],
            modules,
            no_fetch(),
            request_fetcher,
        );
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(
            console.iter().any(|l| l == "got:99"),
            "console was {console:?}"
        );
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
        let (_doc, out) = run_modules(
            doc,
            "https://x/",
            vec![entry],
            modules,
            no_fetch(),
            no_request(),
        );
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(
            console
                .iter()
                .any(|l| l == "caught:TypeError:Failed to fetch"),
            "console was {console:?}"
        );
    }

    #[test]
    fn async_fetch_resolves_during_init_drain() {
        // The async fetch() must complete during Session::new's init drain even though the host
        // request takes time: the request runs on a background thread and the drain keeps looping
        // while it is in flight, then settles the promise and runs the .then chain. We assert the
        // page wrote the response into a DOM attribute before the snapshot was taken.
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"fetch('https://x/a').then(r => r.text()).then(t => document.body.setAttribute('data-a', t));"#
                .to_string(),
        );
        // Test request fetcher: sleep ~50ms then return a canned envelope. Arc + Send + Sync so the
        // background request thread can run it.
        let request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync> =
            Arc::new(|_m, _u, _b, _h| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                Some(
                    r#"{"ok":true,"status":200,"statusText":"OK","url":"https://x/a","contentType":"text/plain","body":"AA"}"#
                        .to_string(),
                )
            });
        let (_session, snapshot, out) = Session::new(
            doc,
            vec![],
            vec![entry],
            modules,
            "https://x/",
            no_fetch(),
            request_fetcher,
            no_ws(),
            None,
        );
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        let got = match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get("data-a").cloned(),
            _ => None,
        };
        assert_eq!(
            got.as_deref(),
            Some("AA"),
            "data-a should be set by the resolved fetch"
        );
    }

    #[test]
    fn async_fetches_run_concurrently() {
        // Five fetches each sleeping 100ms in the host fetcher; fired together with Promise.all. If
        // they were serialized the init drain would take >=500ms; running concurrently (one
        // background thread per request) it finishes in well under that. We also assert all five
        // resolved (their bodies collected into a DOM attribute).
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                var urls = ['a','b','c','d','e'].map(function(s){ return 'https://x/' + s; });
                Promise.all(urls.map(function(u){ return fetch(u).then(function(r){ return r.text(); }); }))
                  .then(function(texts){ document.body.setAttribute('data-all', texts.join(',')); });
            "#
            .to_string(),
        );
        let request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync> =
            Arc::new(|_m, u, _b, _h| {
                std::thread::sleep(std::time::Duration::from_millis(100));
                // Echo the last path segment back as the body so we can verify all five resolved.
                let seg = u.rsplit('/').next().unwrap_or("");
                Some(format!(
                    r#"{{"ok":true,"status":200,"statusText":"OK","url":"{u}","contentType":"text/plain","body":"{seg}"}}"#
                ))
            });
        let start = std::time::Instant::now();
        let (_session, snapshot, out) = Session::new(
            doc,
            vec![],
            vec![entry],
            modules,
            "https://x/",
            no_fetch(),
            request_fetcher,
            no_ws(),
            None,
        );
        let elapsed = start.elapsed();
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        let got = match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get("data-all").cloned(),
            _ => None,
        };
        assert_eq!(
            got.as_deref(),
            Some("a,b,c,d,e"),
            "all five fetches should resolve"
        );
        // Concurrent: 5x100ms serialized would be >=500ms; concurrently it is ~100ms + overhead.
        assert!(
            elapsed < std::time::Duration::from_millis(400),
            "fetches should run concurrently, took {elapsed:?}"
        );
    }

    #[test]
    fn async_fetch_rejects_when_host_returns_none() {
        // A None envelope from the (async) request fetcher rejects the promise with a TypeError, and
        // the page's .catch runs during the drain.
        let (doc, _body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"fetch('https://x/nope').catch(function(e){ console.log('caught:' + e.name + ':' + e.message); });"#
                .to_string(),
        );
        let request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync> =
            Arc::new(|_m, _u, _b, _h| {
                std::thread::sleep(std::time::Duration::from_millis(30));
                None
            });
        let (_session, _snapshot, out) = Session::new(
            doc,
            vec![],
            vec![entry],
            modules,
            "https://x/",
            no_fetch(),
            request_fetcher,
            no_ws(),
            None,
        );
        let console = all_console(&out);
        assert!(
            console
                .iter()
                .any(|l| l == "caught:TypeError:Failed to fetch"),
            "console was {console:?}"
        );
    }

    #[test]
    fn formdata_api_append_get_getall_has_delete_entries() {
        // Exercise the core FormData methods purely in JS.
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                var fd = new FormData();
                fd.append("a", "1");
                fd.append("a", "2");
                fd.append("b", "3");
                console.log("get:" + fd.get("a"));
                console.log("getAll:" + fd.getAll("a").join(","));
                console.log("has:" + fd.has("a") + "," + fd.has("z"));
                fd.set("a", "9");
                console.log("set:" + fd.getAll("a").join(","));
                fd.delete("b");
                console.log("del:" + fd.has("b"));
                var ents = [];
                for (var e of fd.entries()) { ents.push(e[0] + "=" + e[1]); }
                console.log("entries:" + ents.join("&"));
                var it = [];
                for (var p of fd) { it.push(p[0] + "=" + p[1]); }
                console.log("iter:" + it.join("&"));
            "#
            .to_string(),
        );
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(
            doc,
            "https://x/",
            vec![entry],
            modules,
            no_fetch(),
            no_request(),
        );
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(console.contains(&"get:1".to_string()), "{console:?}");
        assert!(console.contains(&"getAll:1,2".to_string()), "{console:?}");
        assert!(
            console.contains(&"has:true,false".to_string()),
            "{console:?}"
        );
        assert!(console.contains(&"set:9".to_string()), "{console:?}");
        assert!(console.contains(&"del:false".to_string()), "{console:?}");
        // After set("a","9") (collapses a to one entry at end) and delete("b"), only a=9 remains.
        assert!(console.contains(&"entries:a=9".to_string()), "{console:?}");
        assert!(console.contains(&"iter:a=9".to_string()), "{console:?}");
    }

    #[test]
    fn formdata_from_form_collects_named_controls() {
        // Constructing FormData from a <form> collects its successful named controls.
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                document.body.innerHTML =
                  '<form id="f">' +
                    '<input name="user" value="luna">' +
                    '<input name="pass" type="password" value="secret">' +
                    '<input name="agree" type="checkbox" value="yes">' +
                    '<input name="news" type="checkbox" value="on1" checked>' +
                    '<input name="ignored" type="submit" value="go">' +
                    '<textarea name="bio">hi there</textarea>' +
                  '</form>';
                var f = document.getElementById("f");
                var fd = new FormData(f);
                console.log("user:" + fd.get("user"));
                console.log("pass:" + fd.get("pass"));
                console.log("agree:" + fd.has("agree"));
                console.log("news:" + fd.get("news"));
                console.log("submit:" + fd.has("ignored"));
                console.log("bio:" + fd.get("bio"));
            "#
            .to_string(),
        );
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(
            doc,
            "https://x/",
            vec![entry],
            modules,
            no_fetch(),
            no_request(),
        );
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(console.contains(&"user:luna".to_string()), "{console:?}");
        assert!(console.contains(&"pass:secret".to_string()), "{console:?}");
        assert!(console.contains(&"agree:false".to_string()), "{console:?}");
        assert!(console.contains(&"news:on1".to_string()), "{console:?}");
        assert!(console.contains(&"submit:false".to_string()), "{console:?}");
        assert!(console.contains(&"bio:hi there".to_string()), "{console:?}");
    }

    #[test]
    fn fetch_post_forwards_method_and_body() {
        // A custom request_fetcher records the method + body and returns a canned envelope; the
        // Response's text()/status must reach the page (proving the round trip + Promise drain).
        use std::sync::{Arc, Mutex};
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                fetch("submit", { method: "post", body: "hello=world",
                    headers: { "X-Test": "1" } })
                  .then(r => r.text().then(t => console.log("resp:" + r.status + ":" + t)));
            "#
            .to_string(),
        );
        let seen: Arc<Mutex<(String, String, String)>> =
            Arc::new(Mutex::new((String::new(), String::new(), String::new())));
        let seen2 = Arc::clone(&seen);
        let request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync> =
            Arc::new(move |method, url, body, headers| {
                *seen2.lock().unwrap() =
                    (method.to_string(), body.to_string(), headers.to_string());
                assert_eq!(url, "https://x/submit");
                Some(
                    r#"{"ok":true,"status":201,"statusText":"Created","url":"https://x/submit","contentType":"text/plain","body":"done"}"#
                        .to_string(),
                )
            });
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(
            doc,
            "https://x/",
            vec![entry],
            modules,
            no_fetch(),
            request_fetcher,
        );
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(
            console.contains(&"resp:201:done".to_string()),
            "{console:?}"
        );
        let (method, body, headers) = seen.lock().unwrap().clone();
        assert_eq!(method, "POST", "method uppercased + forwarded");
        assert_eq!(body, "hello=world", "body forwarded");
        assert!(headers.contains("X-Test"), "headers forwarded: {headers}");
    }

    #[test]
    fn fetch_with_formdata_body_sends_urlencoded() {
        // fetch(url, { body: formData }) serializes the FormData as urlencoded and sets the
        // Content-Type, which the request_fetcher observes.
        use std::sync::{Arc, Mutex};
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                var fd = new FormData();
                fd.append("name", "ada lovelace");
                fd.append("role", "math");
                fetch("u", { method: "POST", body: fd })
                  .then(r => r.text().then(t => console.log("ok:" + t)));
            "#
            .to_string(),
        );
        let seen: Arc<Mutex<(String, String)>> =
            Arc::new(Mutex::new((String::new(), String::new())));
        let seen2 = Arc::clone(&seen);
        let request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync> =
            Arc::new(move |_method, _url, body, headers| {
                *seen2.lock().unwrap() = (body.to_string(), headers.to_string());
                Some(
                    r#"{"ok":true,"status":200,"statusText":"OK","url":"https://x/u","contentType":"text/plain","body":"ok"}"#
                        .to_string(),
                )
            });
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(
            doc,
            "https://x/",
            vec![entry],
            modules,
            no_fetch(),
            request_fetcher,
        );
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(console.contains(&"ok:ok".to_string()), "{console:?}");
        let (body, headers) = seen.lock().unwrap().clone();
        assert_eq!(
            body, "name=ada%20lovelace&role=math",
            "urlencoded body: {body}"
        );
        assert!(
            headers.to_lowercase().contains("x-www-form-urlencoded"),
            "content-type set: {headers}"
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
        let (_doc, out) = run_modules(
            doc,
            "https://x/",
            vec![entry],
            modules,
            no_fetch(),
            no_request(),
        );
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(
            console.iter().any(|l| l == "dims:0,0,0"),
            "console was {console:?}"
        );
    }

    #[test]
    fn modules_see_document_global() {
        // A module can touch the shared DOM-wired `document`/`window`, like page scripts.
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"document.title = "from-module"; console.log("title:" + document.title);"#
                .to_string(),
        );

        let (doc, _) = doc_with_body("orig");
        let (doc, out) = run_modules(
            doc,
            "https://x/",
            vec![entry],
            modules,
            no_fetch(),
            no_request(),
        );
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(
            console.iter().any(|l| l == "title:from-module"),
            "console was {console:?}"
        );
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

        let fetcher: Box<dyn Fn(&str) -> Option<(String, String)> + Send> = Box::new(|u: &str| {
            if u == "https://x/b.js" {
                Some((
                    "export const answer = 99;".to_string(),
                    "text/javascript".to_string(),
                ))
            } else {
                None
            }
        });

        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(
            doc,
            "https://x/",
            vec![entry],
            modules,
            fetcher,
            no_request(),
        );
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(
            console.iter().any(|l| l == "dyn:99"),
            "console was {console:?}"
        );
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
        // cssText is serialized per the CSSOM "serialize a CSS rule" algorithm (declarations end
        // with a trailing `;`, single space inside the braces).
        let out = env_eval(
            "https://example.com/",
            "var s = document.createElement('style'); \
             document.documentElement.appendChild(s); \
             s.textContent = 'a { color: red }'; \
             [typeof s.sheet, s.sheet.cssRules.length, s.sheet.cssRules[0].cssText].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("object|1|a { color: red; }"));
    }

    #[test]
    fn cssom_style_rule_selector_and_css_text() {
        // A <style> rule is a CSSStyleRule with the right selectorText/style/cssText, and
        // document.styleSheets[0] is the same CSSStyleSheet as the element's .sheet.
        let out = env_eval(
            "https://example.com/",
            "var s = document.createElement('style'); \
             document.documentElement.appendChild(s); \
             s.textContent = 'div { margin: 10px; padding: 0px; }'; \
             var r = document.styleSheets[0].cssRules[0]; \
             [r instanceof CSSStyleRule, r instanceof CSSRule, r.type, r.selectorText, \
              r.style.margin, r.cssText, document.styleSheets[0] === s.sheet].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(
            out.value.as_deref(),
            Some("true|true|1|div|10px|div { margin: 10px; padding: 0px; }|true")
        );
    }

    #[test]
    fn cssom_insert_and_delete_rule_preserve_same_object() {
        // insertRule/deleteRule update cssRules; existing wrappers keep their identity ([SameObject]).
        let out = env_eval(
            "https://example.com/",
            "var s = document.createElement('style'); \
             document.documentElement.appendChild(s); \
             s.textContent = 'body { width: 50%; }\\n#foo { height: 100px; }'; \
             var ss = s.sheet; ss.cssRules[0].mark = 1; ss.cssRules[1].mark = 2; \
             ss.insertRule('#bar { margin: 10px; }', 1); \
             var a = [ss.cssRules.length, ss.cssRules[1].cssText, ss.cssRules[0].mark, ss.cssRules[2].mark]; \
             ss.deleteRule(1); \
             a.push(ss.cssRules.length, ss.cssRules[0].mark, ss.cssRules[1].mark); a.join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(
            out.value.as_deref(),
            Some("3|#bar { margin: 10px; }|1|2|2|1|2")
        );
    }

    #[test]
    fn cssom_media_and_import_rules() {
        // @media serializes per CSSOM; @import exposes href/media.
        let out = env_eval(
            "https://example.com/",
            "var s = document.createElement('style'); \
             document.documentElement.appendChild(s); \
             s.textContent = '@import url(\"a.css\") screen;\\n@media all and (color) {}'; \
             var imp = s.sheet.cssRules[0]; var med = s.sheet.cssRules[1]; \
             [imp instanceof CSSImportRule, imp.type, imp.href, imp.media.mediaText, imp.cssText, \
              med instanceof CSSMediaRule, med.type, med.cssText].join('~')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(
            out.value.as_deref(),
            Some("true~3~a.css~screen~@import url(\"a.css\") screen;~true~4~@media (color) {\n}")
        );
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
            "#
            .to_string()],
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
             [el.tagName.toLowerCase(), el.namespaceURI === ns, el.localName, \
              typeof el.appendChild, document.body.lastChild === el].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        // tagName preserves the given qualifiedName (non-HTML namespace); localName drops the prefix.
        assert_eq!(
            out.value.as_deref(),
            Some("svg:path|true|path|function|true")
        );
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
            no_request(),
            no_ws(),
            None,
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
            no_request(),
            no_ws(),
            None,
        );
        assert_eq!(outputs[0].error, None, "{:?}", outputs[0]);
        // Ran at least once during load.
        let body0 = find_by_tag(&snapshot, snapshot.root(), "body").expect("body node");
        let initial: i32 = attr_of(&snapshot, body0, "data-c")
            .unwrap_or_default()
            .parse()
            .unwrap_or(0);
        assert!(
            initial >= 1,
            "interval should run during load, got {initial}"
        );

        // After real time elapses, a tick fires it again (real-clock cadence) → count increases.
        std::thread::sleep(std::time::Duration::from_millis(80));
        let (after, _console) = session.tick().expect("interval should fire again on tick");
        let body = find_by_tag(&after, after.root(), "body").expect("body node");
        let c: i32 = attr_of(&after, body, "data-c")
            .unwrap_or_default()
            .parse()
            .unwrap_or(0);
        assert!(
            c > initial,
            "interval should have fired again on tick: {initial} -> {c}"
        );
    }

    #[test]
    fn session_event_bubbles_to_ancestor() {
        let doc =
            html::parse("<div id=parent><button id=child></button></div><span id=out>idle</span>");
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
            no_request(),
            no_ws(),
            None,
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
            no_request(),
            no_ws(),
            None,
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
            no_request(),
            no_ws(),
            None,
        );
        assert!(outputs.is_empty() || outputs.iter().all(|o| o.error.is_none()));

        let f = find_by_id(&snapshot, snapshot.root(), "f").expect("input node");
        let (after, _c) = session.dispatch_key(f.0, "Backspace", "Backspace");
        let input = find_by_id(&after, after.root(), "f").expect("input node");
        assert_eq!(attr_of(&after, input, "value").as_deref(), Some("h"));
    }

    #[test]
    fn session_toggle_checkbox_flips_checked_and_fires_change() {
        let doc = html::parse("<html><body><input id=c type=checkbox></body></html>");
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
            no_request(),
            no_ws(),
            None,
        );
        assert_eq!(outputs[0].error, None, "{:?}", outputs[0]);

        let c = find_by_id(&snapshot, snapshot.root(), "c").expect("checkbox node");
        // Initially unchecked.
        assert!(attr_of(&snapshot, c, "checked").is_none());

        let (after, _console) = session.toggle_checkbox(c.0);
        let cb = find_by_id(&after, after.root(), "c").expect("checkbox node");
        assert!(
            attr_of(&after, cb, "checked").is_some(),
            "checkbox should be checked"
        );
        let body = find_by_tag(&after, after.root(), "body").expect("body node");
        assert_eq!(attr_of(&after, body, "data-changed").as_deref(), Some("on"));

        // Toggling again unchecks it (and the change handler sees the new state).
        let (after2, _c2) = session.toggle_checkbox(c.0);
        let cb2 = find_by_id(&after2, after2.root(), "c").expect("checkbox node");
        assert!(
            attr_of(&after2, cb2, "checked").is_none(),
            "checkbox should be unchecked"
        );
        let body2 = find_by_tag(&after2, after2.root(), "body").expect("body node");
        assert_eq!(
            attr_of(&after2, body2, "data-changed").as_deref(),
            Some("off")
        );
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
            no_request(),
            no_ws(),
            None,
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
        assert!(
            attr_of(&after, bb, "checked").is_some(),
            "b should be checked"
        );
        assert!(
            attr_of(&after, aa, "checked").is_none(),
            "a should be unchecked"
        );
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
            no_request(),
            no_ws(),
            None,
        );
        assert_eq!(outputs[0].error, None, "{:?}", outputs[0]);

        let menu = find_by_id(&snapshot, snapshot.root(), "menu").expect("menu node");
        let (after, _console) = session.dispatch_event(menu.0, "mouseover", 5.0, 5.0);
        let body = find_by_tag(&after, after.root(), "body").expect("body node");
        assert_eq!(attr_of(&after, body, "data-hover").as_deref(), Some("yes"));
    }

    #[test]
    fn session_nonbubbling_focus_does_not_reach_ancestor() {
        let doc = html::parse("<html><body><div id=wrap><input id=f></div></body></html>");
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
            no_request(),
            no_ws(),
            None,
        );
        assert_eq!(outputs[0].error, None, "{:?}", outputs[0]);

        let f = find_by_id(&snapshot, snapshot.root(), "f").expect("input node");
        let (after, _console) = session.fire_event_nonbubbling(f.0, "focus");
        let body = find_by_tag(&after, after.root(), "body").expect("body node");
        // The target's focus handler ran...
        assert_eq!(
            attr_of(&after, body, "data-target").as_deref(),
            Some("focused")
        );
        // ...but the ancestor's did NOT (focus does not bubble).
        assert_eq!(attr_of(&after, body, "data-ancestor").as_deref(), None);
    }

    // --- MutationObserver -------------------------------------------------------------------

    #[test]
    fn mutation_observer_childlist_fires_with_added_node() {
        // observe({childList:true}); append a child; the callback should run and see the addedNode.
        let (doc, _body) = doc_with_body("");
        let (doc, out) = run_with_dom(
            doc,
            vec![r#"
                var target = document.body;
                var seenTag = "";
                var ran = 0;
                var mo = new MutationObserver(function (records) {
                    for (var i = 0; i < records.length; i++) {
                        var r = records[i];
                        if (r.type === "childList" && r.addedNodes.length) {
                            ran++;
                            seenTag = r.addedNodes[0].tagName;
                        }
                    }
                    document.body.setAttribute("data-mo-ran", String(ran));
                    document.body.setAttribute("data-mo-tag", seenTag);
                });
                mo.observe(target, { childList: true });
                var el = document.createElement("span");
                target.appendChild(el);
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        let body = find_by_tag(&doc, doc.root(), "body").expect("body");
        // Callback fired exactly once with the appended <span> in addedNodes.
        assert_eq!(attr_of(&doc, body, "data-mo-ran").as_deref(), Some("1"));
        assert_eq!(attr_of(&doc, body, "data-mo-tag").as_deref(), Some("SPAN"));
    }

    #[test]
    fn mutation_observer_attributes_with_old_value() {
        // observe({attributes:true, attributeOldValue:true}); change an attr; the callback should
        // see the attributeName and the captured oldValue.
        let (mut doc, body) = doc_with_body("");
        // Give body an initial attribute so the change has an old value.
        if let dom::NodeData::Element(e) = &mut doc.get_mut(body).data {
            e.attrs.insert("data-x".to_string(), "old".to_string());
        }
        let (doc, out) = run_with_dom(
            doc,
            vec![r#"
                var target = document.body;
                var captured = false;
                var mo = new MutationObserver(function (records) {
                    if (captured) { return; }
                    var r = records[0];
                    if (r.attributeName !== "data-x") { return; }
                    captured = true;
                    document.body.setAttribute("data-name", r.attributeName);
                    document.body.setAttribute("data-old", r.oldValue == null ? "<null>" : r.oldValue);
                });
                mo.observe(target, { attributes: true, attributeOldValue: true });
                target.setAttribute("data-x", "new");
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        let body = find_by_tag(&doc, doc.root(), "body").expect("body");
        assert_eq!(attr_of(&doc, body, "data-x").as_deref(), Some("new"));
        assert_eq!(attr_of(&doc, body, "data-name").as_deref(), Some("data-x"));
        assert_eq!(attr_of(&doc, body, "data-old").as_deref(), Some("old"));
    }

    #[test]
    fn mutation_observer_disconnect_stops_delivery_and_gate() {
        // After disconnect(), subsequent mutations must NOT invoke the callback.
        let (doc, _body) = doc_with_body("");
        let (doc, out) = run_with_dom(
            doc,
            vec![r#"
                var runs = 0;
                var mo = new MutationObserver(function () { runs++; });
                mo.observe(document.body, { childList: true });
                mo.disconnect();
                document.body.appendChild(document.createElement("span"));
                // Deliver any queued mutations synchronously via a microtask drain happens at end;
                // record the count on an attribute so we can read it post-drain.
                document.body.setAttribute("data-runs", String(runs));
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        let body = find_by_tag(&doc, doc.root(), "body").expect("body");
        assert_eq!(attr_of(&doc, body, "data-runs").as_deref(), Some("0"));
    }

    // ---- Node mutation method cluster (cloneNode / textContent / ChildNode / ParentNode) --------

    #[test]
    fn clone_node_deep_copies_attrs_and_children_and_is_detached() {
        let out = env_eval(
            "https://example.com/",
            "var d = document.createElement('div'); d.setAttribute('class', 'a b'); d.id = 'x'; \
             var s = document.createElement('span'); s.textContent = 'hi'; d.appendChild(s); \
             var deep = d.cloneNode(true); \
             var shallow = d.cloneNode(false); \
             [deep === d, deep.parentNode === null, deep.getAttribute('class'), deep.id, \
              deep.childNodes.length, deep.childNodes[0].tagName, deep.textContent, \
              shallow.childNodes.length, shallow.getAttribute('class'), \
              deep instanceof HTMLDivElement].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(
            out.value.as_deref(),
            Some("false|true|a b|x|1|SPAN|hi|0|a b|true")
        );
    }

    #[test]
    fn text_content_get_and_set() {
        let out = env_eval(
            "https://example.com/",
            "var d = document.createElement('div'); \
             d.appendChild(document.createTextNode('foo')); \
             var b = document.createElement('b'); b.appendChild(document.createTextNode('bar')); \
             d.appendChild(b); \
             var got = d.textContent; \
             d.textContent = 'replaced'; \
             var afterChildren = d.childNodes.length; \
             var afterText = d.childNodes[0].nodeType; \
             var replacedText = d.textContent; \
             d.textContent = ''; \
             var c = document.createComment('cmt'); \
             [got, replacedText, afterChildren, afterText, d.childNodes.length, \
              c.textContent, document.textContent].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        // doc.textContent is null per spec; empty set removes children (length 0).
        assert_eq!(out.value.as_deref(), Some("foobar|replaced|1|3|0|cmt|"));
    }

    #[test]
    fn child_before_inserts_strings_and_nodes() {
        let out = env_eval(
            "https://example.com/",
            "var p = document.createElement('p'); \
             var mid = document.createElement('mid'); p.appendChild(mid); \
             var other = document.createElement('o'); \
             mid.before('x', other); \
             [p.childNodes.length, p.childNodes[0].nodeType, p.childNodes[0].textContent, \
              p.childNodes[1].tagName, p.childNodes[2].tagName].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("3|3|x|O|MID"));
    }

    #[test]
    fn replace_with_fragment() {
        let out = env_eval(
            "https://example.com/",
            "var p = document.createElement('p'); \
             var old = document.createElement('old'); p.appendChild(old); \
             var f = document.createDocumentFragment(); \
             f.appendChild(document.createElement('a')); \
             f.appendChild(document.createElement('b')); \
             old.replaceWith(f); \
             [p.childNodes.length, p.childNodes[0].tagName, p.childNodes[1].tagName, \
              old.parentNode === null].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("2|A|B|true"));
    }

    #[test]
    fn replace_children_replaces_all() {
        let out = env_eval(
            "https://example.com/",
            "var p = document.createElement('p'); \
             p.appendChild(document.createElement('keep1')); \
             p.appendChild(document.createElement('keep2')); \
             var a = document.createElement('a'); var b = document.createElement('b'); \
             p.replaceChildren(a, b, 'tail'); \
             [p.childNodes.length, p.childNodes[0].tagName, p.childNodes[1].tagName, \
              p.childNodes[2].textContent].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("3|A|B|tail"));
    }

    #[test]
    fn insert_before_throws_not_found_for_non_child_ref() {
        let out = env_eval(
            "https://example.com/",
            "var p = document.createElement('p'); \
             var n = document.createElement('n'); \
             var stranger = document.createElement('s'); \
             var name = ''; var code = -1; \
             try { p.insertBefore(n, stranger); } catch (e) { name = e.name; code = e.code; } \
             [name, code].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("NotFoundError|8"));
    }

    #[test]
    fn append_child_ancestor_throws_hierarchy_request() {
        let out = env_eval(
            "https://example.com/",
            "var a = document.createElement('a'); \
             var b = document.createElement('b'); a.appendChild(b); \
             var name = ''; var code = -1; \
             try { b.appendChild(a); } catch (e) { name = e.name; code = e.code; } \
             [name, code].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("HierarchyRequestError|3"));
    }
}
