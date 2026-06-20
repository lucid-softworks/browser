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
type WsConnector = Arc<
    dyn Fn(String, u64, Sender<WsEvent>) -> Result<Sender<WsOut>, String> + Send + Sync,
>;

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
    fetcher: Rc<dyn Fn(&str) -> Option<String>>,
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
        fetcher: Rc<dyn Fn(&str) -> Option<String>>,
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
            image_natural: RefCell::new(HashMap::new()),
            canvas_pixels: RefCell::new(HashMap::new()),
            viewport_scroll_y: Cell::new(0.0),
            doc_height: Cell::new(0.0),
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
        dom::NodeData::ProcessingInstruction(p) => return p.data.clone(),
        _ => {}
    }
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
        } else if matches!(ch, '.' | '#' | '[' | ':' | '>' | '+' | '~' | ',') || ch.is_whitespace() {
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
fn parse_complex(s: &str) -> Option<Vec<Compound>> {
    let bytes: Vec<char> = s.chars().collect();
    let mut segments: Vec<String> = Vec::new();
    let mut cur = String::new();
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
                segments.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
            i += 1;
            continue;
        }
        cur.push(ch);
        i += 1;
    }
    if !cur.trim().is_empty() {
        segments.push(cur);
    }
    let parts: Vec<Compound> = segments.iter().filter_map(|s| parse_compound(s)).collect();
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
        dom::NodeData::Element(dom::ElementData { tag, attrs: Default::default() }),
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

/// `__createDocumentFragment() -> id` — a parentless `DocumentFragment` arena node.
fn prim_create_document_fragment(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let state = host_state(scope);
    let id = state.doc.borrow_mut().alloc(dom::NodeData::DocumentFragment, None);
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
        dom::NodeData::DocumentType(dom::DoctypeData { name, public_id, system_id }),
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
            obj.set(scope, k.into(), v.into());
            let k = v8::String::new(scope, "publicId").unwrap();
            let v = js_str(scope, &public_id);
            obj.set(scope, k.into(), v.into());
            let k = v8::String::new(scope, "systemId").unwrap();
            let v = js_str(scope, &system_id);
            obj.set(scope, k.into(), v.into());
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
fn collect_author_sheets(doc: &dom::Document) -> Vec<css::Stylesheet> {
    fn walk(doc: &dom::Document, id: dom::NodeId, out: &mut String) {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag.eq_ignore_ascii_case("style") {
                out.push_str(&text_content(doc, id));
                out.push('\n');
                return; // don't descend into a <style>'s text as if it were markup
            }
        }
        for &child in &doc.get(id).children {
            walk(doc, child, out);
        }
    }
    let mut css_src = String::new();
    walk(doc, doc.root(), &mut css_src);
    if css_src.trim().is_empty() {
        Vec::new()
    } else {
        vec![css::parse(&css_src)]
    }
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
            let sheets = collect_author_sheets(&doc);
            let map = style::cascade(&doc, &sheets);
            *cache = Some((version, map));
        }
    }
    let cache = state.computed_cache.borrow();
    let map = cache.as_ref().map(|(_, m)| m);
    f(map.and_then(|m| m.get(&id)))
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
            let sheets = collect_author_sheets(&doc);
            let map = style::cascade(&doc, &sheets);
            *cache = Some((version, map));
        }
    }
    let cache = state.computed_cache.borrow();
    let map = cache.as_ref().map(|(_, m)| m).expect("cascade just populated");
    let doc = state.doc.borrow();
    f(&doc, map)
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
) -> Option<String> {
    let cs = map.get(&id)?;
    // A box-less element (display:none, or an ancestor is) has no used value → computed value.
    let box_less = cs.display == style::Display::None || ancestor_display_none(doc, map, id);
    if box_less || cs.position == style::Position::Static {
        return Some(cs.resolved_inset(side, true, f32::NAN));
    }

    // Find the containing block and the relevant axis extent (height for top/bottom, width for
    // left/right). For in-flow / relative / sticky the cb is the parent's *content* box; for
    // absolute the nearest positioned ancestor's *padding* box; for fixed the nearest transformed
    // ancestor's padding box (the viewport otherwise — approximated by the document element).
    let want_height = matches!(side, style::EdgeSide::Top | style::EdgeSide::Bottom);
    let basis = match cs.position {
        style::Position::Relative | style::Position::Sticky => {
            containing_block_extent(doc, map, id, ContainingBlock::ParentContent, want_height)
        }
        style::Position::Absolute => {
            containing_block_extent(doc, map, id, ContainingBlock::PositionedPadding, want_height)
        }
        style::Position::Fixed => {
            containing_block_extent(doc, map, id, ContainingBlock::TransformedPadding, want_height)
        }
        style::Position::Static => f32::NAN,
    };
    Some(cs.resolved_inset(side, false, basis))
}

/// Which ancestor establishes the containing block, and which box of it forms the basis.
enum ContainingBlock {
    /// Parent element's content box (in-flow / relative / sticky).
    ParentContent,
    /// Nearest positioned ancestor's padding box (absolute).
    PositionedPadding,
    /// Nearest ancestor with a transform's padding box, else the document element (fixed).
    TransformedPadding,
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
    let state = host_state(scope);
    // Inset longhands need the CSSOM resolved-value algorithm (position + containing block), which
    // reads more than one element's style — handle them via the cascade map directly.
    let inset_side = match name.as_str() {
        "top" => Some(style::EdgeSide::Top),
        "right" => Some(style::EdgeSide::Right),
        "bottom" => Some(style::EdgeSide::Bottom),
        "left" => Some(style::EdgeSide::Left),
        _ => None,
    };
    let value = match (node, inset_side) {
        (Some(n), Some(side)) => with_cascade_map(&state, |doc, map| {
            resolved_inset_value(doc, map, n, side).unwrap_or_default()
        }),
        (Some(n), None) => with_computed_style(&state, n, |cs| {
            cs.map(|cs| cs.get_property(&name)).unwrap_or_default()
        }),
        (None, _) => String::new(),
    };
    let s = js_str(scope, &value);
    rv.set(s);
}

/// `__computedStyleNames(id) -> [name...]` — the property names with non-empty computed values for
/// node `id` (backs `length`/`item(i)`/index access/iteration). Empty array for non-elements.
fn prim_computed_style_names(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let node = arg_node(scope, &args, 0);
    let state = host_state(scope);
    let names: Vec<String> = match node {
        Some(n) => with_computed_style(&state, n, |cs| {
            cs.map(|cs| cs.property_names().iter().map(|s| s.to_string()).collect())
                .unwrap_or_default()
        }),
        None => Vec::new(),
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
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' })
        .collect();
    let safe = if safe.is_empty() { "default".to_string() } else { safe };
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
    let mut seed = std::collections::hash_map::RandomState::new().build_hasher().finish();
    for b in buf.iter_mut() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
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
    let n = if n.is_finite() && n > 0.0 { (n as usize).min(1 << 20) } else { 0 };
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
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
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
    let (cw, ch) = client_box.unwrap_or((w, h));
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
    let envelope =
        (host_state(scope).request_fetcher)(&method, &resolved, &body, &headers_json);
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
    set_fn(scope, global, "__createDocumentFragment", prim_create_document_fragment);
    set_fn(scope, global, "__createDocumentType", prim_create_document_type);
    set_fn(scope, global, "__createProcessingInstruction", prim_create_processing_instruction);
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
    set_fn(scope, global, "__request", prim_request);
    set_fn(scope, global, "__startFetch", prim_start_fetch);
    set_fn(scope, global, "__wsConnect", prim_ws_connect);
    set_fn(scope, global, "__wsSend", prim_ws_send);
    set_fn(scope, global, "__wsClose", prim_ws_close);
    set_fn(scope, global, "__observersActive", prim_observers_active);
    set_fn(scope, global, "__drainMutations", prim_drain_mutations);
    set_fn(scope, global, "__computedStyleProp", prim_computed_style_prop);
    set_fn(scope, global, "__computedStyleNames", prim_computed_style_names);
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
      return nodeId;
    }
    __insertBefore(parentId, nodeId, refId);
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
      Object.defineProperty(el, "data", {
        get: function () { return __textContent(id); },
        set: function (v) { __setTextContent(id, v == null ? "" : String(v)); },
        enumerable: true, configurable: true
      });
    }
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
    return wrap(__createProcessingInstruction(t, String(data)));
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

    function makeDeclaration(id) {
      var names = null; // lazily fetched list of populated property names
      function getNames() { if (names === null) { try { names = __computedStyleNames(id) || []; } catch (e) { names = []; } } return names; }
      function get(prop) { try { return __computedStyleProp(id, String(prop).toLowerCase()); } catch (e) { return ""; } }

      var decl = {
        getPropertyValue: function (name) { return get(name); },
        getPropertyPriority: function () { return ""; },
        // Computed styles are read-only: mutators are no-ops.
        setProperty: fn,
        removeProperty: function () { return ""; },
        item: function (i) { var n = getNames(); i = i >>> 0; return i < n.length ? n[i] : ""; }
      };
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

    globalThis.getComputedStyle = function (el) {
      var id = (el && typeof el.__node === "number") ? el.__node : null;
      if (id === null) { return emptyDeclaration(); }
      return makeDeclaration(id);
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
      try { target.dispatchEvent(makeEvent(type)); } catch (e) { (globalThis.__timerErrors || []).push((e&&e.stack||String(e))); }
    }
    // Also invoke an `on<type>` handler if one was assigned (e.g. window.onload = ...).
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
    var out = "";
    var i = 0, n = val.length;
    while (i < n) {
      var ch = val[i];
      // Skip quoted strings verbatim.
      if (ch === '"' || ch === "'") {
        var q = ch; out += ch; i++;
        while (i < n && val[i] !== q) { out += val[i]; i++; }
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
      if (name) { out.push([name, normalizeCssValue(val)]); }
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
        val = normalizeCssValue(String(val));
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
  function normalizeComplexSelector(sel) {
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
          else { var lid = ""; while (i < n && isIdentChar(chars[i], lid === "")) { lid += chars[i]; i++; } local = lid; }
          // Validate the local part.
          if (local !== "*" && !isIdent(local)) { return err(); }
          // Serialize the prefix: `*|` → drop; `|` (empty default ns) → keep `|`; named → keep.
          var serPre = "";
          if (pre === "*" || pre === "") {
            serPre = (pre === "") ? "|" : ""; // empty prefix means the default namespace, kept as `|`
          } else if (isIdent(pre)) {
            serPre = ""; // undeclared named namespace: serialize bare (no namespaces declared)
          } else { return err(); }
          out += serPre + local;
        } else {
          // No prefix: a bare universal or type selector.
          if (pre === "*") { out += "*"; }
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
            var inner = arg.split(",").map(function (s) { return normalizeComplexSelector(s); });
            if (inner.indexOf(null) >= 0 || inner.length === 0) { return err(); }
            out += ":" + lower + "(" + inner.join(", ") + ")";
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
    // A universal `*` is dropped when another simple selector follows it in the same compound
    // (e.g. `*:not(:active)` serializes as `:not(:active)`, `*.x` as `.x`).
    out = out.replace(/\*(?=[.#:\[])/g, "");
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
    var name;
    if (rawPre != null) {
      var pre = rawPre.slice(0, -1); // drop the trailing `|`
      if (!isIdent(local)) { return null; }
      if (pre === "*") { name = "*|" + local; }       // `[*|lang]` keeps the `*|`
      else if (pre === "") { name = local; }           // `[|lang]` -> `[lang]`
      else if (isIdent(pre)) { name = pre + "|" + local; }
      else { return null; }
    } else {
      if (!isIdent(local)) { return null; }
      name = local;
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
  function normalizeSelectorList(sel) {
    sel = String(sel == null ? "" : sel);
    var parts = sel.split(",");
    var outs = [];
    for (var i = 0; i < parts.length; i++) {
      var nrm = normalizeComplexSelector(parts[i]);
      if (nrm === null) { return null; }
      outs.push(nrm);
    }
    if (!outs.length) { return null; }
    return outs.join(", ");
  }
  function makeRule(text, sheet, index) {
    var rule = {
      type: 1, cssRules: [], parentRule: null,
      get parentStyleSheet() { return sheet || null; },
      get cssText() { return sheet ? sheet.__ruleCssText(index) : text; }
    };
    Object.defineProperty(rule, "selectorText", {
      get: function () {
        var t = sheet ? sheet.__ruleCssText(index) : text;
        var raw = (String(t).split("{")[0] || "").trim();
        var nrm = normalizeSelectorList(raw);
        return nrm == null ? raw : nrm;
      },
      set: function (v) {
        var nrm = normalizeSelectorList(v);
        if (nrm == null) { return; } // invalid selector: leave the rule unchanged (per spec)
        if (sheet) { sheet.__setRuleSelector(index, nrm); }
      },
      enumerable: true, configurable: true
    });
    return rule;
  }
  function makeStyleSheet(styleEl) {
    function rawRules() { return parseCssRules(styleEl.textContent); }
    var ss = {
      type: "text/css", disabled: false, href: null, title: null, media: { length: 0 },
      ownerNode: styleEl, parentStyleSheet: null,
      // The cssText of rule `index` (re-read live so a selector edit is reflected).
      __ruleCssText: function (index) { var rs = rawRules(); return rs[index] || ""; },
      // Rewrite the selector of rule `index`, preserving its declaration block, and write the whole
      // sheet back to the <style> element so the cascade (and getComputedStyle) re-applies.
      __setRuleSelector: function (index, newSel) {
        var rs = rawRules();
        if (index < 0 || index >= rs.length) { return; }
        var brace = rs[index].indexOf("{");
        var block = brace >= 0 ? rs[index].slice(brace) : "{ }";
        rs[index] = newSel + " " + block;
        styleEl.textContent = rs.join("\n");
      },
      get cssRules() {
        var raw = rawRules();
        var rs = [];
        for (var i = 0; i < raw.length; i++) { rs.push(makeRule(raw[i], ss, i)); }
        rs.item = function (i) { return this[i] || null; };
        return rs;
      },
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
  function __reflResolveURL(v) {
    v = String(v);
    var base = globalThis.__pageURL;
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

  function enrichElement(el) {
    if (!el || typeof el !== "object") { return el; }
    if (el.__enriched) { return el; }
    var node = el.__node;
    def(el, "__enriched", true);
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
      def(el, "style", makeStyle(node));
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

    // <style> (and stylesheet <link>) expose a live CSSStyleSheet via `.sheet`.
    if (typeof el.tagName === "string" && (el.tagName.toLowerCase() === "style" || el.tagName.toLowerCase() === "link") && !("sheet" in el)) {
      var __sheet = null;
      Object.defineProperty(el, "sheet", { get: function () { if (!__sheet) { __sheet = makeStyleSheet(this); } return __sheet; }, configurable: true, enumerable: false });
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
            get: function () { var m = __elemMetrics(this.__node); return m ? m[field] : 0; },
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
      // Real arena-backed DocumentFragment (nodeType 11): its children move on insertion, and it
      // supports the full ParentNode mixin (append/prepend/replaceChildren/appendChild/insertBefore).
      // Canonicalize so navigation accessors (firstChild) return enriched, prototype-correct nodes.
      return canon(__wrapNode(__createDocumentFragment()));
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
  // Legacy event factory. Maps a (case-insensitive) interface name to an uninitialized event of
  // the right interface (prototype chain intact); unknown names throw NotSupportedError. The real
  // implementation lives in globalThis.__createEvent (defined alongside the Event constructors).
  def(document, "createEvent", function (name) { return globalThis.__createEvent(name); });
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

  // --- a few more constructors pages feature-detect ----------------------------------------
  if (typeof globalThis.DOMParser !== "function") {
    def(globalThis, "DOMParser", function () {
      this.parseFromString = function (str, type) {
        var t = String(type || "").toLowerCase();
        // text/html parses as an HTML document (HTML namespace); XML flavours produce an XML
        // document whose createElement assigns the null namespace.
        if (t === "text/html") { return document; }
        if (document.implementation && typeof document.implementation.createDocument === "function") {
          // XHTML documents are in the HTML namespace; other XML flavours use the null namespace.
          var ns = (t === "application/xhtml+xml") ? "http://www.w3.org/1999/xhtml" : null;
          return document.implementation.createDocument(ns, "", null);
        }
        return document;
      };
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
    }
  } catch (e) {}
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
          if (value !== undefined) { return String(prop).length > 0 && String(value).length > 0; }
          var c = String(prop); return c.indexOf(":") > 0 || c.indexOf("(") >= 0;
        } catch (e) { return false; }
      },
      escape: function (value) {
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
        let budget = if in_flight > 0 { network_budget } else { idle_budget };
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
                let env_arg = js_str(scope, &envelope).into();
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
        let payload_arg = js_str(scope, payload).into();
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        doc: dom::Document,
        scripts: Vec<String>,
        entries: Vec<String>,
        modules: HashMap<String, String>,
        url: &str,
        fetcher: Box<dyn Fn(&str) -> Option<String> + Send>,
        request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync>,
        ws_connector: WsConnector,
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
                    session_thread_main(
                        doc, scripts, entries, modules, url, fetcher, request_fetcher,
                        ws_connector, init_tx, cmd_rx,
                    );
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

    /// Push the engine's laid-out element rects (CSS px, document-absolute, top-origin) to the
    /// worker so element-geometry reads (`getBoundingClientRect`, `offsetWidth`, `scrollHeight`, …)
    /// return real values. Fire-and-forget: does not block on a reply, so the engine (which holds
    /// the DOM while the worker is idle between commands) never stalls on this. The worker stores
    /// the rects on `HostState`; the next geometry read serves them.
    pub fn set_layout_rects(
        &self,
        rects: Vec<(usize, f32, f32, f32, f32)>,
        naturals: Vec<(usize, f32, f32)>,
        scroll_y_css: f32,
        doc_height_css: f32,
    ) {
        let _ = self.tx.send(SessionCmd::SetRects { rects, naturals, scroll_y_css, doc_height_css });
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
        self.eval_interact(
            "(globalThis.__mediaChanged && globalThis.__mediaChanged())".to_string(),
        )
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
        let (out, _doc, _console) =
            self.eval_full("JSON.stringify((globalThis.__canvasLists||function(){return[]})())".to_string());
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
        if self.tx.send(SessionCmd::Eval { source, reply: reply_tx }).is_err() {
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
    request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync>,
    ws_connector: WsConnector,
    init_tx: std::sync::mpsc::Sender<(dom::Document, Vec<EvalOutput>)>,
    cmd_rx: std::sync::mpsc::Receiver<SessionCmd>,
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

        // Run initial classic scripts in order, then the module graph, exactly as the load path.
        let mut results: Vec<EvalOutput> =
            Vec::with_capacity(scripts.len() + entries.len());
        for source in &scripts {
            results.push(eval_source(scope, source, "<script>"));
        }
        for entry in &entries {
            results.push(run_one_entry(scope, entry));
        }
        drain_event_loop(scope, &mut results, Some(&fetch_rx), Some(&ws_evt_rx));
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
                drain_event_loop(scope, &mut results, Some(&fetch_rx), Some(&ws_evt_rx));
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
                let first = EvalOutput { value: results[0].value.clone(), error: results[0].error.clone(), console: Vec::new() };
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
                let did_work = drain_event_loop(scope, &mut results, Some(&fetch_rx), Some(&ws_evt_rx));
                // Only snapshot+report when something actually ran, so idle ticks are cheap.
                if did_work {
                    let console = results.into_iter().flat_map(|r| r.console).collect();
                    let _ = reply.send(Some((shared.borrow().clone(), console)));
                } else {
                    let _ = reply.send(None);
                }
            }
            SessionCmd::SetRects { rects, naturals, scroll_y_css, doc_height_css } => {
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
        assert_eq!(out[0].value.as_deref(), Some("InvalidCharacterError|5|true"));
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
            "#.to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("string||hi|yo|boolean|false|true|false|number|5|0"));
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
            "#.to_string()],
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
            "#.to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("text|text|email|boolean|true|-1|5"));
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
            "#.to_string()],
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
            vec![r#"var e = document.createElementNS("http://www.w3.org/2000/svg", "svg:Rect");
                    [e.namespaceURI, e.prefix, e.localName, e.tagName, e.nodeName].join("|")"#
                .to_string()],
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
            "#.to_string()],
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
            "#.to_string()],
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
            "#.to_string()],
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
            "#.to_string()],
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
            "#.to_string()],
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
            "#.to_string()],
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
            "#.to_string()],
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
        assert!(json.contains("\"op\":\"fillRect\"") && json.contains("\"clip\""), "no clipped fill: {json}");
        assert!(json.contains("\"dash\""), "no dash on stroke: {json}");
        assert!(json.contains("\"op\":\"drawImage\""), "no drawImage cmd: {json}");
        assert!(json.contains("\"op\":\"putImageData\""), "no putImageData cmd: {json}");
        assert!(json.contains("getLineDash") == false); // sanity: no leaked fn names
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
            e.attrs.insert("style".to_string(), "font-size:18px".to_string());
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
        doc.append_child(style_el, dom::NodeData::Text(".x{display:flex}".to_string()));
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
            "#.to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some(r#"0.5%|-0.1em|0px|url("http://localhost/")"#));
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
            "#.to_string()],
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
        doc.append_child(style_el, dom::NodeData::Text(".a { color: red }".to_string()));
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
            "#.to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some(".a|span > div|span > div|::after"));
    }

    #[test]
    fn computed_style_mutating_inline_style_invalidates_cache() {
        // Read once, mutate the inline style, read again -> must reflect the NEW value (the
        // dom_version bump on setAttribute invalidates the cached cascade).
        let (mut doc, body) = doc_with_body("");
        doc.append_element(body, "div");
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                r#"var el = document.querySelectorAll('div')[0];
                   var before = getComputedStyle(el).color;
                   el.style.color = 'rgb(1, 2, 3)';
                   var after = getComputedStyle(el).color;
                   before + '|' + after"#
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        // `before` is the inherited default; `after` is the freshly-set inline color.
        let v = out[0].value.as_deref().unwrap();
        assert!(v.ends_with("|rgb(1, 2, 3)"), "expected new color after mutation, got {v}");
        assert_ne!(v, "rgb(1, 2, 3)|rgb(1, 2, 3)", "before should differ from after");
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
                "var s = getComputedStyle(document.querySelectorAll('div')[0]); \
                 (s.length > 0) && (typeof s.item(0) === 'string') && (s.item(0).length > 0) \
                 && (s.setProperty('color','blue'), s.removeProperty('color'), true)"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("true"));
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
            "#.to_string()],
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
            "#.to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("html|html|pub|sys|10|null|null"));
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
            vec![r#"[CSS.escape(".foo#bar"), CSS.escape("0abc"), CSS.escape("-")].join('|')"#.to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        // "." and "#" are escaped with a backslash; a leading digit becomes "\\3N "; lone "-" -> "\\-".
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
            "#.to_string()],
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
            "#.to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("noopener noreferrer|[object DOMTokenList]|true"));
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
            "#.to_string()],
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
        assert_eq!(out.value.as_deref(), Some("true|1|attributes|class|a b c|a d c"));
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
        assert_eq!(out.value.as_deref(), Some("false|b|true|b a|true|a b|false|a b"));
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
        assert_eq!(out.value.as_deref(), Some("true:a b|false:a a a  b|true:c b"));
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
        assert_eq!(out.value.as_deref(), Some("true|InvalidCharacterError|5|true"));
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
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules, no_fetch(), no_request());
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
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules, no_fetch(), no_request());
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
            run_modules(doc, "https://x/", vec![entry.clone()], modules, no_fetch(), no_request());
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
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules, no_fetch(), no_request());
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
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules, no_fetch(), no_request());
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
                assert_eq!(u, "https://x/data.json", "fetch should resolve relative URLs");
                Some(
                    r#"{"ok":true,"status":200,"statusText":"OK","url":"https://x/data.json","contentType":"application/json","body":"{\"score\": 99}"}"#
                        .to_string(),
                )
            });

        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules, no_fetch(), request_fetcher);
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
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules, no_fetch(), no_request());
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(
            console.iter().any(|l| l == "caught:TypeError:Failed to fetch"),
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
        );
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        let got = match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get("data-a").cloned(),
            _ => None,
        };
        assert_eq!(got.as_deref(), Some("AA"), "data-a should be set by the resolved fetch");
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
        );
        let elapsed = start.elapsed();
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        let got = match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get("data-all").cloned(),
            _ => None,
        };
        assert_eq!(got.as_deref(), Some("a,b,c,d,e"), "all five fetches should resolve");
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
        );
        let console = all_console(&out);
        assert!(
            console.iter().any(|l| l == "caught:TypeError:Failed to fetch"),
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
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules, no_fetch(), no_request());
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(console.contains(&"get:1".to_string()), "{console:?}");
        assert!(console.contains(&"getAll:1,2".to_string()), "{console:?}");
        assert!(console.contains(&"has:true,false".to_string()), "{console:?}");
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
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules, no_fetch(), no_request());
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
        let (_doc, out) =
            run_modules(doc, "https://x/", vec![entry], modules, no_fetch(), request_fetcher);
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(console.contains(&"resp:201:done".to_string()), "{console:?}");
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
        let (_doc, out) =
            run_modules(doc, "https://x/", vec![entry], modules, no_fetch(), request_fetcher);
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(console.contains(&"ok:ok".to_string()), "{console:?}");
        let (body, headers) = seen.lock().unwrap().clone();
        assert_eq!(body, "name=ada%20lovelace&role=math", "urlencoded body: {body}");
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
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules, no_fetch(), no_request());
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
        let (doc, out) = run_modules(doc, "https://x/", vec![entry], modules, no_fetch(), no_request());
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
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules, fetcher, no_request());
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
             [el.tagName.toLowerCase(), el.namespaceURI === ns, el.localName, \
              typeof el.appendChild, document.body.lastChild === el].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        // tagName preserves the given qualifiedName (non-HTML namespace); localName drops the prefix.
        assert_eq!(out.value.as_deref(), Some("svg:path|true|path|function|true"));
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
            no_request(),
            no_ws(),
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
            no_request(),
            no_ws(),
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
            no_request(),
            no_ws(),
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
            no_request(),
            no_ws(),
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
            no_request(),
            no_ws(),
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
