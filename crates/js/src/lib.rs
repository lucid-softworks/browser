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

mod inner_text;

pub(crate) use std::cell::Cell;
pub(crate) use std::cell::RefCell;
pub(crate) use std::collections::HashMap;
pub(crate) use std::rc::Rc;
pub(crate) use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
pub(crate) use std::sync::mpsc::{Receiver, Sender};
pub(crate) use std::sync::Arc;
pub(crate) use std::sync::Once;

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

mod dom_helpers;
mod eval_loop;
mod iframe;
mod modules;
mod primitives;
mod runtime;
mod selector;
mod session;
mod style_query;
mod worker;

pub(crate) use dom_helpers::*;
pub use eval_loop::*;
pub use iframe::*;
pub use modules::*;
pub use primitives::*;
pub use runtime::*;
pub(crate) use selector::*;
pub use session::*;
pub(crate) use style_query::*;
pub use worker::*;

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

    #[test]
    fn structured_clone_deep_copies_cycles_and_throws_on_uncloneable() {
        // Replaces the old JSON-roundtrip stub: must deep-copy Maps, preserve cycles and shared
        // ArrayBuffer aliasing, and raise DataCloneError on a function — none of which the stub did.
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var r = [];
                    var m = new Map([["a", 1]]); var mc = structuredClone(m);
                    r.push(mc instanceof Map && mc.get("a") === 1 && mc !== m);
                    var c = {}; c.self = c; var cc = structuredClone(c);
                    r.push(cc.self === cc && cc !== c);
                    var buf = new ArrayBuffer(4); var v = { a: new Uint8Array(buf), b: new Uint8Array(buf) };
                    var vc = structuredClone(v); vc.a[0] = 9;
                    r.push(vc.b[0] === 9 && vc.a.buffer === vc.b.buffer && vc.a.buffer !== buf);
                    var threw = false; try { structuredClone(function () {}); } catch (e) { threw = e.name === "DataCloneError"; }
                    r.push(threw);
                    r.join("|")"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("true|true|true|true"));
    }

    #[test]
    fn custom_element_lifecycle_callbacks_fire_in_order() {
        // connectedCallback already worked; this also exercises the new attributeChangedCallback
        // (only for observedAttributes) and disconnectedCallback (via the removeChild wrapper).
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"globalThis.__log = [];
                    class XEl {
                      static get observedAttributes() { return ["data-x"]; }
                      connectedCallback() { __log.push("connected"); }
                      disconnectedCallback() { __log.push("disconnected"); }
                      attributeChangedCallback(n, o, v) { __log.push("attr:" + n + ":" + o + ":" + v); }
                    }
                    customElements.define("x-el", XEl);
                    var e = document.createElement("x-el");
                    document.body.appendChild(e);
                    e.setAttribute("data-x", "1");
                    e.setAttribute("data-x", "2");
                    e.setAttribute("data-y", "z");   // not observed -> no callback
                    e.removeAttribute("data-x");
                    e.remove();
                    __log.join("|")"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("connected|attr:data-x:null:1|attr:data-x:1:2|attr:data-x:2:null|disconnected")
        );
    }

    #[test]
    fn text_encoder_decoder_utf8_correctness() {
        // encodeInto: real read/written counts, never a partial sequence. TextDecoder: U+FFFD on
        // invalid input, fatal throws, BOM stripped unless ignoreBOM, streaming carries a partial.
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var r = [];
                    var te = new TextEncoder();
                    var d1 = new Uint8Array(10); var i1 = te.encodeInto("ab€", d1);
                    r.push("ei_full:" + i1.read + ":" + i1.written);
                    var d2 = new Uint8Array(2); var i2 = te.encodeInto("a€", d2);
                    r.push("ei_tight:" + i2.read + ":" + i2.written);
                    var d3 = new Uint8Array(4); var i3 = te.encodeInto("😀", d3);
                    r.push("ei_surr:" + i3.read + ":" + i3.written);
                    var d4 = new Uint8Array(3); var i4 = te.encodeInto("😀", d4);
                    r.push("ei_nofit:" + i4.read + ":" + i4.written);
                    r.push("dec:" + new TextDecoder().decode(new Uint8Array([0x68, 0xC3, 0xA9])));
                    r.push("bad:" + new TextDecoder().decode(new Uint8Array([0xFF])));
                    r.push("bom:" + new TextDecoder().decode(new Uint8Array([0xEF, 0xBB, 0xBF, 0x41])));
                    r.push("ibom:" + new TextDecoder("utf-8", { ignoreBOM: true }).decode(new Uint8Array([0xEF, 0xBB, 0xBF, 0x41])).length);
                    var threw = false;
                    try { new TextDecoder("utf-8", { fatal: true }).decode(new Uint8Array([0xFF])); } catch (e) { threw = e instanceof TypeError; }
                    r.push("fatal:" + threw);
                    var sd = new TextDecoder();
                    var p1 = sd.decode(new Uint8Array([0xC3]), { stream: true });
                    var p2 = sd.decode(new Uint8Array([0xA9]));
                    r.push("stream:" + p1.length + ":" + p2);
                    r.join("|")"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("ei_full:3:5|ei_tight:1:1|ei_surr:2:4|ei_nofit:0:0|dec:hé|bad:\u{fffd}|bom:A|ibom:2|fatal:true|stream:0:é")
        );
    }

    #[test]
    fn dom_parser_text_html_returns_independent_document() {
        // Regression: the text/html path used to return the LIVE document and ignore the input
        // string. It must parse into a fresh document (content distributed to head/body) that does
        // not leak into the live tree.
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var d = new DOMParser().parseFromString(
                      "<!DOCTYPE html><html><head><title>T</title></head><body><p id=x>hi</p></body></html>",
                      "text/html");
                    var r = [];
                    r.push("notlive:" + (d !== document));
                    var p = d.body.querySelector("p#x");
                    r.push("body:" + (p ? p.textContent : "null"));
                    var ti = d.querySelector("title");
                    r.push("title:" + (ti ? ti.textContent : "null"));
                    r.push("leak:" + document.body.querySelector("p#x"));
                    r.join("|")"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("notlive:true|body:hi|title:T|leak:null")
        );
    }

    #[test]
    fn indexeddb_open_store_index_cursor_roundtrip() {
        // Exercises the in-memory IndexedDB end to end: open + onupgradeneeded creates a keyPath
        // store and an index, adds rows; then a readwrite txn puts a row, and a readonly txn reads
        // back via get/count/index.getAll/cursor. Callbacks log to console (captured during drain).
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var L = function (s) { console.log(s); };
                    var req = indexedDB.open("testdb", 1);
                    req.onupgradeneeded = function (e) {
                      var s = req.result.createObjectStore("people", { keyPath: "id" });
                      s.createIndex("byAge", "age");
                      s.add({ id: 1, name: "Ann", age: 30 });
                      s.add({ id: 2, name: "Bob", age: 25 });
                      s.add({ id: 3, name: "Cy", age: 30 });
                      L("upgrade:" + e.oldVersion + "->" + e.newVersion);
                    };
                    req.onsuccess = function () {
                      var db = req.result;
                      var w = db.transaction("people", "readwrite");
                      w.objectStore("people").put({ id: 4, name: "Di", age: 25 });
                      w.oncomplete = function () {
                        var s = db.transaction("people", "readonly").objectStore("people");
                        s.get(2).onsuccess = function (ev) { L("get2:" + ev.target.result.name); };
                        s.count().onsuccess = function (ev) { L("count:" + ev.target.result); };
                        s.index("byAge").getAll(25).onsuccess = function (ev) {
                          L("age25:" + ev.target.result.map(function (r) { return r.name; }).sort().join(","));
                        };
                        var names = [];
                        s.openCursor().onsuccess = function (ev) {
                          var c = ev.target.result;
                          if (c) { names.push(c.value.name); c.continue(); } else { L("cursor:" + names.join(",")); }
                        };
                      };
                    };
                    "ok""#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].console,
            vec![
                "upgrade:0->1".to_string(),
                "get2:Bob".to_string(),
                "count:4".to_string(),
                "age25:Bob,Di".to_string(),
                "cursor:Ann,Bob,Cy,Di".to_string(),
            ]
        );
    }

    #[test]
    fn web_crypto_subtle_digest_and_hmac_known_vectors() {
        // Assert SubtleCrypto against published test vectors — the surest correctness check for a
        // hand-written hash. SHA-1/256/384/512 of "abc" and HMAC-SHA256(key="key", message=…).
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var L = function (s) { console.log(s); };
                    var hex = function (buf) { return Array.prototype.map.call(new Uint8Array(buf), function (b) { return (b + 0x100).toString(16).slice(1); }).join(""); };
                    var enc = new TextEncoder();
                    crypto.subtle.digest("SHA-256", enc.encode("abc")).then(function (h) { L("sha256:" + hex(h)); });
                    crypto.subtle.digest("SHA-1", enc.encode("abc")).then(function (h) { L("sha1:" + hex(h)); });
                    crypto.subtle.digest("SHA-512", enc.encode("abc")).then(function (h) { L("sha512:" + hex(h)); });
                    crypto.subtle.digest("SHA-384", enc.encode("abc")).then(function (h) { L("sha384:" + hex(h)); });
                    crypto.subtle.importKey("raw", enc.encode("key"), { name: "HMAC", hash: "SHA-256" }, false, ["sign", "verify"]).then(function (k) {
                      return crypto.subtle.sign("HMAC", k, enc.encode("The quick brown fox jumps over the lazy dog")).then(function (sig) { L("hmac:" + hex(sig)); });
                    });
                    "ok""#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].console,
            vec![
                "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_string(),
                "sha1:a9993e364706816aba3e25717850c26c9cd0d89d".to_string(),
                "sha512:ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f".to_string(),
                "sha384:cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7".to_string(),
                "hmac:f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8".to_string(),
            ]
        );
    }

    #[test]
    fn web_crypto_aes_ctr_vector_and_cbc_roundtrip() {
        // AES-CTR against NIST SP800-38A F.5.1 (AES-128): validates the cipher core + counter mode.
        // AES-CBC: encrypt-then-decrypt must round-trip back to the plaintext (validates dec path).
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var L = function (s) { console.log(s); };
                    var hex = function (buf) { return Array.prototype.map.call(new Uint8Array(buf), function (b) { return (b + 0x100).toString(16).slice(1); }).join(""); };
                    var fromHex = function (h) { var a = new Uint8Array(h.length / 2); for (var i = 0; i < a.length; i++) { a[i] = parseInt(h.substr(i * 2, 2), 16); } return a; };
                    var key = fromHex("2b7e151628aed2a6abf7158809cf4f3c");
                    var ctr = fromHex("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff");
                    var pt = fromHex("6bc1bee22e409f96e93d7e117393172a");
                    crypto.subtle.importKey("raw", key, { name: "AES-CTR" }, false, ["encrypt", "decrypt"]).then(function (k) {
                      return crypto.subtle.encrypt({ name: "AES-CTR", counter: ctr, length: 128 }, k, pt).then(function (ct) { L("ctr:" + hex(ct)); });
                    });
                    crypto.subtle.importKey("raw", key, { name: "AES-CBC" }, false, ["encrypt", "decrypt"]).then(function (k) {
                      var msg = new TextEncoder().encode("hello AES-CBC world!");
                      var iv = fromHex("000102030405060708090a0b0c0d0e0f");
                      return crypto.subtle.encrypt({ name: "AES-CBC", iv: iv }, k, msg).then(function (ct) {
                        return crypto.subtle.decrypt({ name: "AES-CBC", iv: iv }, k, ct).then(function (back) { L("cbc:" + new TextDecoder().decode(back)); });
                      });
                    });
                    "ok""#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].console,
            vec![
                "ctr:874d6191b620e3261bef6864990db6ce".to_string(),
                "cbc:hello AES-CBC world!".to_string(),
            ]
        );
    }

    #[test]
    fn stubbed_apis_no_longer_throw() {
        // Previously-missing APIs surfaced by the WPT sweep now respond instead of throwing
        // "X is not a function": Node.getRootNode/isSameNode/moveBefore, element scroll/visibility/
        // popover, document.execCommand/getAnimations/startViewTransition, Selection.setBaseAndExtent,
        // and SubtleCrypto.supports.
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var r = [];
                    var div = document.createElement("div"); document.body.appendChild(div);
                    r.push("root:" + (div.getRootNode() === document));
                    r.push("same:" + div.isSameNode(div) + ":" + div.isSameNode(document.body));
                    var a = document.createElement("span"), b = document.createElement("span");
                    div.appendChild(b); div.moveBefore(a, b);
                    r.push("move:" + (div.firstChild === a));
                    div.scrollTo(0, 0); div.scroll(); div.scrollBy(1, 1);
                    r.push("vis:" + div.checkVisibility());
                    div.showPopover(); div.hidePopover();
                    r.push("toggle:" + div.togglePopover());
                    r.push("exec:" + document.execCommand("bold"));
                    r.push("anims:" + document.getAnimations().length);
                    r.push("svt:" + (typeof document.startViewTransition(function () {}).finished.then));
                    r.push("sup256:" + crypto.subtle.supports("digest", "SHA-256"));
                    r.push("supMd5:" + crypto.subtle.supports("digest", "MD5"));
                    r.push("supAes:" + SubtleCrypto.supports("encrypt", "AES-CBC"));
                    var t = document.createTextNode("hello"); div.appendChild(t);
                    var sel = getSelection(); sel.setBaseAndExtent(t, 0, t, 3);
                    r.push("sel:" + sel.toString());
                    r.join("|")"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("root:true|same:true:false|move:true|vis:true|toggle:false|exec:false|anims:0|svt:function|sup256:true|supMd5:false|supAes:true|sel:hel")
        );
    }

    #[test]
    fn form_parentnode_media_storage_stubs() {
        // sweep4 batch: ParentNode insertion (prepend/append), <input> stepUp/stepDown/select/
        // setSelectionRange, <video> play/addTextTrack, and StorageEvent.initStorageEvent.
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var r = [];
                    var p = document.createElement("div"); document.body.appendChild(p);
                    var a = document.createElement("span"); a.textContent = "A";
                    var b = document.createElement("span"); b.textContent = "B";
                    p.append(a); p.prepend(b);
                    r.push("pre:" + p.firstChild.textContent + ":" + p.lastChild.textContent);
                    var inp = document.createElement("input"); inp.setAttribute("step", "2"); inp.value = "5";
                    inp.stepUp(); r.push("up:" + inp.value);
                    inp.stepDown(3); r.push("down:" + inp.value);
                    inp.value = "hello"; inp.select(); r.push("sel:" + inp.selectionStart + ":" + inp.selectionEnd);
                    inp.setSelectionRange(1, 3); r.push("ssr:" + inp.selectionStart + ":" + inp.selectionEnd);
                    var v = document.createElement("video");
                    r.push("play:" + (typeof v.play().then) + ":" + v.addTextTrack("subtitles").kind);
                    var se = new StorageEvent("storage");
                    se.initStorageEvent("storage", false, false, "k", "o", "n", "http://x/", null);
                    r.push("se:" + se.key + ":" + se.oldValue + ":" + se.newValue);
                    r.join("|")"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("pre:B:A|up:7|down:1|sel:0:5|ssr:1:3|play:function:subtitles|se:k:o:n")
        );
    }

    #[test]
    fn range_extract_and_delete_contents() {
        // Range.extractContents moves the selected content into a fragment (trimming char-data
        // boundaries); deleteContents leaves the same tree state without returning it.
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var r = [];
                    var p = document.createElement("p"); p.textContent = "hello world"; document.body.appendChild(p);
                    var rng = document.createRange(); rng.setStart(p.firstChild, 3); rng.setEnd(p.firstChild, 8);
                    var frag = rng.extractContents();
                    r.push("ex:" + frag.textContent + ":" + p.textContent);
                    var div = document.createElement("div"); div.innerHTML = "<b>aa</b><i>bb</i><u>cc</u>"; document.body.appendChild(div);
                    var rng2 = document.createRange(); rng2.setStartBefore(div.children[1]); rng2.setEndAfter(div.children[1]);
                    rng2.deleteContents();
                    r.push("del:" + div.innerHTML);
                    var div2 = document.createElement("div"); div2.innerHTML = "<span>x</span><span>y</span>"; document.body.appendChild(div2);
                    var rng3 = document.createRange(); rng3.selectNode(div2.children[0]);
                    var frag3 = rng3.extractContents();
                    r.push("ext2:" + frag3.childNodes.length + ":" + div2.innerHTML);
                    r.join("|")"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("ex:lo wo:helrld|del:<b>aa</b><u>cc</u>|ext2:1:<span>y</span>")
        );
    }

    #[test]
    fn selection_caret_api() {
        // Selection.collapse/extend/selectAllChildren/collapseToStart over the Range model — the
        // editing tests lean heavily on these (collapse alone was ~531 "is not a function" hits).
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var r = [];
                    var p = document.createElement("p"); p.textContent = "hello world"; document.body.appendChild(p);
                    var t = p.firstChild, sel = getSelection();
                    sel.collapse(t, 3); r.push("c:" + sel.anchorOffset + ":" + sel.type);
                    sel.extend(t, 8); r.push("e:" + sel.toString());
                    sel.selectAllChildren(p); r.push("a:" + sel.type + ":" + (sel.anchorNode === p));
                    sel.collapseToStart(); r.push("s:" + sel.type);
                    r.join("|")"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("c:3:Caret|e:lo wo|a:Range:true|s:Caret")
        );
    }

    #[test]
    fn more_stubbed_apis_respond() {
        // Tail of the "is not a function" sweep: TextEvent.initTextEvent, Element.attachInternals
        // (minimal ElementInternals), and Animation.commitStyles.
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var r = [];
                    var te = new TextEvent("t"); te.initTextEvent("textInput", true, false, window, "hi");
                    r.push("te:" + te.type + ":" + te.data);
                    var ai = document.createElement("x-y").attachInternals();
                    ai.setFormValue("v"); r.push("ai:" + ai.checkValidity() + ":" + (typeof ai.states.add));
                    var an = document.body.animate({ opacity: [0, 1] }, 1); an.commitStyles(); an.persist();
                    r.push("an:ok");
                    r.join("|")"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("te:textInput:hi|ai:true:function|an:ok")
        );
    }

    #[test]
    fn legacy_event_init_and_document_node_methods() {
        // Second "is not a function" batch: legacy Event init* methods set their members, and the
        // document (not an element) answers getRootNode/isSameNode.
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var r = [];
                    var me = new MouseEvent("x");
                    me.initMouseEvent("click", true, true, window, 1, 10, 20, 30, 40, false, false, true, false, 0, null);
                    r.push("me:" + me.type + ":" + me.clientX + ":" + me.clientY + ":" + me.shiftKey + ":" + me.bubbles);
                    var ue = new UIEvent("y"); ue.initUIEvent("custom", true, false, window, 5);
                    r.push("ue:" + ue.type + ":" + ue.detail);
                    var ke = new KeyboardEvent("z"); ke.initKeyboardEvent("keydown", true, true, window, "Enter", 0, true, false, false, false);
                    r.push("ke:" + ke.type + ":" + ke.key + ":" + ke.ctrlKey);
                    r.push("doc:" + (document.getRootNode() === document) + ":" + document.isSameNode(document));
                    r.join("|")"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("me:click:30:40:true:true|ue:custom:5|ke:keydown:Enter:true|doc:true:true")
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
    fn hyperlink_username_password_reflect_userinfo() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var a = document.createElement("a");
                a.href = "http://user:pass@example.test/path";
                var r = [a.username, a.password];
                a.username = "new user";
                a.password = "p@ss word";
                r.push(a.username, a.password, a.href);

                var area = document.createElement("area");
                area.href = "https://area:secret@example.test/map";
                r.push(area.username, area.password);

                a.href = "mailto:name@example.test";
                a.username = "ignored";
                a.password = "also-ignored";
                r.push(a.href, a.username, a.password);
                r.join("|");
            "#
            .to_string()],
            "https://example.com/base/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("user|pass|new%20user|p%40ss%20word|http://new%20user:p%40ss%20word@example.test/path|area|secret|mailto:name@example.test||")
        );
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
    fn aria_element_reflection_single_and_array() {
        // ARIA element reflection: ariaActiveDescendantElement (single Element) and the
        // FrozenArray<Element> family. Covers content-attribute fallback, IDL set emptying the
        // content attribute + storing the reference, null clearing it, and a setAttribute reset.
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var p = document.createElement("div");
                var a = document.createElement("div"); a.id = "a";
                var b = document.createElement("div"); b.id = "b";
                document.body.appendChild(p);
                document.body.appendChild(a);
                document.body.appendChild(b);
                var r = [];
                // Content-attribute fallback resolves the IDREF to the element.
                p.setAttribute("aria-activedescendant", "a");
                r.push(p.ariaActiveDescendantElement === a);     // true
                // IDL set stores the reference and empties the content attribute.
                p.ariaActiveDescendantElement = b;
                r.push(p.getAttribute("aria-activedescendant")); // ""
                r.push(p.ariaActiveDescendantElement === b);     // true
                // setAttribute clears the explicit reference (falls back to IDREF lookup).
                p.setAttribute("aria-activedescendant", "a");
                r.push(p.ariaActiveDescendantElement === a);     // true
                // null removes the content attribute.
                p.ariaActiveDescendantElement = null;
                r.push(p.hasAttribute("aria-activedescendant")); // false
                // FrozenArray family: parsed IDREF list, caching invariant, and type checking.
                p.setAttribute("aria-describedby", "a b");
                r.push(p.ariaDescribedByElements.length);        // 2
                r.push(p.ariaDescribedByElements === p.ariaDescribedByElements); // true (cached)
                p.ariaDescribedByElements = [b];
                r.push(p.getAttribute("aria-describedby"));      // ""
                r.push(p.ariaDescribedByElements[0] === b);      // true
                var threw = false;
                try { p.ariaControlsElements = [1]; } catch (e) { threw = e instanceof TypeError; }
                r.push(threw);                                   // true
                r.join("|");
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("true||true|true|false|2|true||true|true")
        );
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
    fn webkit_prefixed_event_handlers_use_prefixed_event_types() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var div = document.createElement("div");
                var hits = [];
                var lower = false;
                var upper = false;
                div.addEventListener("webkitAnimationStart", function () { hits.push("listener"); });
                div.addEventListener("webkitanimationstart", function () { lower = true; });
                div.addEventListener("WEBKITANIMATIONSTART", function () { upper = true; });
                var initial = [
                    div.onanimationstart === null,
                    div.onwebkitanimationstart === null
                ].join("|");
                div.onanimationstart = function () { hits.push("unprefixed"); };
                div.onwebkitanimationstart = function () { hits.push("prefixed"); };
                var distinct = div.onanimationstart !== div.onwebkitanimationstart;
                div.dispatchEvent(new AnimationEvent("webkitAnimationStart"));
                div.dispatchEvent(new AnimationEvent("animationstart"));
                [
                    initial,
                    distinct,
                    hits.join("|"),
                    lower,
                    upper,
                    window.onwebkittransitionend === null,
                    document.onwebkitanimationend === null
                ].join(",")
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("true|true,true,listener|prefixed|unprefixed,false,false,true,true")
        );
    }

    #[test]
    fn dispatch_event_rejects_invalid_and_active_events() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var invalidType = false;
                try { document.dispatchEvent(null); }
                catch (e) { invalidType = e instanceof TypeError; }
                var plainObjectType = false;
                try { document.dispatchEvent({ type: "ping" }); }
                catch (e) { plainObjectType = e instanceof TypeError; }

                var uninitialized = document.createEvent("Event");
                var invalidState = false;
                try { document.dispatchEvent(uninitialized); }
                catch (e) { invalidState = e.name === "InvalidStateError"; }

                var target = document.createElement("div");
                var event = new Event("ping");
                var nestedInvalidState = false;
                target.addEventListener("ping", function () {
                    try { target.dispatchEvent(event); }
                    catch (e) { nestedInvalidState = e.name === "InvalidStateError"; }
                });
                var first = target.dispatchEvent(event);
                var second = target.dispatchEvent(event);
                [invalidType, plainObjectType, invalidState, nestedInvalidState,
                 first, second].join(",")
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("true,true,true,true,true,true")
        );
    }

    #[test]
    fn dispatch_event_returns_false_only_when_canceled() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var target = document.createElement("div");
                target.addEventListener("cancel", function (event) { event.preventDefault(); });
                var uncancelable = target.dispatchEvent(new Event("cancel"));
                var canceled = target.dispatchEvent(new Event("cancel", { cancelable: true }));
                [uncancelable, canceled].join(",")
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("true,false"));
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
                 [s.cursor, s.getPropertyValue('transition')].join(',')"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some(","));
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
    fn huge_css_value_setter_is_linear_not_quadratic() {
        // Regression for grid-template-columns-crash.html: setting a very long property value must
        // not be O(n²). normalizeCssValue previously indexed the growing result rope per number
        // token (`out[out.length-1]`), forcing repeated flattening, so a ~1.8MB value hung the
        // engine (V8 GC thrash -> WebDriver "not responding" -> CRASH). Build a large value and
        // assert the round-trip completes well under the worker budget.
        let (doc, _) = doc_with_body("");
        let (_d, out) = run_with_dom(
            doc,
            vec![
                "var v=''; for (var i=0;i<40000;i++){ v+=' repeat(1000, '+i+'px)'; } \
                 var t0=Date.now(); document.body.style.gridTemplateColumns=v; \
                 var ok = document.body.style.gridTemplateColumns.length > 0; \
                 [ok, (Date.now()-t0) < 4000].join(',')"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        // Both flags true: the value was stored, and the setter finished fast (linear).
        assert_eq!(out[0].value.as_deref(), Some("true,true"));
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
    fn load_clock_does_not_fast_forward_long_timeouts() {
        // The load-time virtual clock fast-forwards to fire pending short timers, but must NOT skip
        // across a far-future timer. Regression for testdriver tests: a page awaiting external input
        // (e.g. test_driver.Actions().send()) leaves only a multi-second testharness timeout pending;
        // fast-forwarding to it would fire "Test timed out" during load. A short (0ms) timer still
        // runs; a long (10s) timer is deferred (fires later on the real clock, not during load).
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"setTimeout(() => console.log("short"), 0);
                    setTimeout(() => console.log("LONG-fired"), 10000);"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        let all: Vec<String> = out.iter().flat_map(|o| o.console.clone()).collect();
        assert!(
            all.iter().any(|l| l == "short"),
            "short timer should run: {all:?}"
        );
        assert!(
            !all.iter().any(|l| l == "LONG-fired"),
            "10s timer must NOT be fast-forwarded during load: {all:?}"
        );
    }

    #[test]
    fn element_animate_returns_animation_with_settling_finished() {
        // Minimal Web Animations: Element.animate() returns an Animation whose `finished` promise
        // resolves after the effect duration. Unblocks WPT's `waitForCompositorReady`
        // (`body.animate({opacity:[0,1]},{duration:1}).finished`) and any test awaiting an animation
        // to sync a frame, which otherwise error with "animate is not a function".
        let (doc, body) = doc_with_body("");
        let _ = body;
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                "var a = document.body.animate({opacity:[0,1]}, {duration:1}); \
                 a.finished.then(function(){ console.log('finished:' + a.playState); }); \
                 [typeof document.body.animate, a.finished instanceof Promise, \
                  document.body.getAnimations().length].join(',')"
                    .to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("function,true,0"));
        let all: Vec<String> = out.iter().flat_map(|o| o.console.clone()).collect();
        assert!(
            all.iter().any(|l| l == "finished:finished"),
            "animation finished promise did not resolve: {all:?}"
        );
    }

    #[test]
    fn window_post_message_dispatches_message_event() {
        // `window.postMessage` delivers a `message` MessageEvent to the window asynchronously, with
        // data structured-cloned, the window's own origin, and source === window. WPT's
        // testdriver.js (`test_driver.message_test`) relies on this to reach the testharness context;
        // without it, testdriver tests error out at setup with "window.postMessage is not a function".
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![concat!(
                "console.log('typeof=' + typeof window.postMessage);",
                "window.addEventListener('message', function (e) {",
                "  console.log('msg:' + e.data + '|origin=' + e.origin + '|self=' + (e.source === window));",
                "});",
                "window.onmessage = function (e) { console.log('on:' + e.data); };",
                "window.postMessage('hello', '*');"
            )
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        let all: Vec<String> = out.iter().flat_map(|o| o.console.clone()).collect();
        assert!(all.iter().any(|l| l == "typeof=function"), "{all:?}");
        assert!(
            all.iter()
                .any(|l| l == "msg:hello|origin=https://example.com|self=true"),
            "addEventListener('message') did not fire correctly: {all:?}"
        );
        assert!(
            all.iter().any(|l| l == "on:hello"),
            "onmessage did not fire: {all:?}"
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
    fn created_html_document_body_is_live_and_settable() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                function emptyHTMLDocument() {
                    var doc = document.implementation.createHTMLDocument("");
                    doc.removeChild(doc.documentElement);
                    return doc;
                }

                var childless = emptyHTMLDocument();
                var childlessBody = childless.body === null;

                var noBody = emptyHTMLDocument();
                noBody.appendChild(noBody.createElement("html"));
                var emptyHtmlBody = noBody.body === null;

                var doc = emptyHTMLDocument();
                var html = doc.appendChild(doc.createElement("html"));
                var frameset = html.appendChild(doc.createElement("frameset"));
                var body = html.appendChild(doc.createElement("body"));
                var firstBody = doc.body === frameset;
                var replacement = doc.createElement("body");
                doc.body = replacement;

                var noRoot = emptyHTMLDocument();
                var noRootError = "";
                try { noRoot.body = noRoot.createElement("body"); }
                catch (e) { noRootError = e.name; }

                var badType = "";
                try { document.body = "text"; }
                catch (e) { badType = e.name; }

                var badElement = "";
                try { document.body = document.createElement("div"); }
                catch (e) { badElement = e.name; }

                [
                    childlessBody,
                    emptyHtmlBody,
                    firstBody,
                    frameset instanceof HTMLFrameSetElement,
                    frameset.parentNode === null,
                    doc.body === replacement,
                    replacement instanceof HTMLBodyElement,
                    body.parentNode === html,
                    noRootError,
                    badType,
                    badElement
                ].join(",")
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("true,true,true,true,true,true,true,true,HierarchyRequestError,TypeError,HierarchyRequestError")
        );
    }

    #[test]
    fn create_range_is_available_on_every_document() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var foreign = document.implementation.createHTMLDocument("");
                var xml = document.implementation.createDocument(null, null, null);
                var bare = new Document();
                var docs = [document, foreign, xml, bare];
                docs.map(function (doc) {
                    var range = doc.createRange();
                    return typeof doc.createRange === "function" &&
                           range instanceof Range && range instanceof AbstractRange &&
                           range.startContainer === doc && range.startOffset === 0 &&
                           range.endContainer === doc && range.endOffset === 0 &&
                           range.collapsed;
                }).join(",")
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("true,true,true,true"));
    }

    #[test]
    fn foreign_node_owner_document_creates_range_in_that_document() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var foreign = document.implementation.createHTMLDocument("");
                var child = foreign.createElement("p");
                foreign.body.appendChild(child);
                var owner = child.ownerDocument;
                var first = owner.createRange();
                var second = owner.createRange();
                [owner === foreign, first.startContainer === foreign,
                 first.endContainer === foreign, first !== second].join(",")
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("true,true,true,true"));
    }

    #[test]
    fn static_range_constructor_stores_boundary_points() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var div = document.createElement("div");
                div.append("abc", document.createElement("span"), "ghi");
                document.body.appendChild(div);
                var text = div.firstChild;
                var range = new StaticRange({
                    startContainer: div,
                    startOffset: 1,
                    endContainer: text,
                    endOffset: 20
                });
                var collapsed = new StaticRange({
                    startContainer: div,
                    startOffset: 0,
                    endContainer: div,
                    endOffset: 0
                });
                var errors = [];
                function capture(fn) {
                    try { fn(); errors.push("none"); }
                    catch (e) { errors.push(e.name + ":" + (e instanceof DOMException)); }
                }
                capture(function () { new StaticRange(); });
                capture(function () { new StaticRange({ startOffset: 0, endContainer: div, endOffset: 0 }); });
                capture(function () {
                    var dt = document.implementation.createDocumentType("html", "", "");
                    new StaticRange({ startContainer: dt, startOffset: 0, endContainer: div, endOffset: 0 });
                });
                capture(function () {
                    var attr = document.createAttribute("id");
                    new StaticRange({ startContainer: attr, startOffset: 0, endContainer: attr, endOffset: 0 });
                });
                [
                    range instanceof StaticRange,
                    range instanceof AbstractRange,
                    range.startContainer === div,
                    range.startOffset,
                    range.endContainer === text,
                    range.endOffset,
                    range.collapsed,
                    collapsed.collapsed,
                    errors.join("|")
                ].join(",")
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("true,true,true,1,true,20,false,true,TypeError:false|TypeError:false|InvalidNodeTypeError:true|InvalidNodeTypeError:true")
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
    fn location_url_setters_convert_lone_surrogates_to_usvstring() {
        let out = env_eval(
            "https://example.com/base",
            r#"
              var lone = String.fromCharCode(0xd999);
              location.hash = lone;
              var hash = location.hash;
              location.href = "about:blank#" + lone;
              var href = location.href;
              [hash, href].join("|")
            "#,
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(
            out.value.as_deref(),
            Some("#%EF%BF%BD|about:blank#%EF%BF%BD")
        );
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
    fn abort_signal_dispatches_real_event_once() {
        let out = env_eval(
            "https://example.com/",
            "var c = new AbortController(); var onCount = 0; var listenerCount = 0; \
             var validEvent = true; \
             c.signal.onabort = function (event) { \
               onCount++; validEvent = validEvent && event instanceof Event && \
                 event.target === c.signal && event.currentTarget === c.signal; \
             }; \
             c.signal.addEventListener('abort', function (event) { \
               listenerCount++; validEvent = validEvent && event instanceof Event && \
                 event.target === c.signal && event.currentTarget === c.signal; \
             }); \
             c.abort(); c.abort(); \
             [onCount, listenerCount, validEvent].join(',')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("1,1,true"));
    }

    #[test]
    fn websocket_delivery_dispatches_real_event_once() {
        let out = env_eval(
            "https://example.com/",
            "var ws = new WebSocket('ws://example.com/socket'); \
             var onCount = 0; var listenerCount = 0; var validEvent = true; \
             ws.onmessage = function (event) { \
               onCount++; validEvent = validEvent && event instanceof Event && \
                 event.target === ws && event.currentTarget === ws && event.data === 'hello'; \
             }; \
             ws.addEventListener('message', function (event) { \
               listenerCount++; validEvent = validEvent && event instanceof Event && \
                 event.target === ws && event.currentTarget === ws && event.data === 'hello'; \
             }); \
             __wsDeliver(ws.__wsid, 1, 'hello'); \
             [onCount, listenerCount, validEvent].join(',')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("1,1,true"));
    }

    #[test]
    fn service_worker_container_and_interfaces_present() {
        // navigator.serviceWorker is a real ServiceWorkerContainer (EventTarget) exposing the
        // registration methods, with the lifecycle interface objects defined globally.
        let out = env_eval(
            "https://example.com/",
            "var c = navigator.serviceWorker; \
             [typeof c.register, typeof c.getRegistration, typeof c.getRegistrations, \
              c instanceof ServiceWorkerContainer, String(c.controller), typeof c.ready.then, \
              typeof ServiceWorker, typeof ServiceWorkerRegistration, \
              Object.prototype.toString.call(c)].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(
            out.value.as_deref(),
            Some("function|function|function|true|null|function|function|function|[object ServiceWorkerContainer]")
        );
    }

    #[test]
    fn service_worker_register_validation_rejections() {
        // register()/getRegistration() reject synchronously (no script fetch) for a non-http(s)
        // scope scheme and an encoded slash in the scope (TypeError), and for a cross-origin
        // documentURL (SecurityError). The reasons are observed after the microtask drain.
        let (doc, body) = doc_with_body("");
        let (doc, out) = run_with_dom(
            doc,
            vec![r#"
              var b = document.body;
              var rej = function (attr) {
                return function (e) { b.setAttribute(attr, (e && e.name) || String(e)); };
              };
              navigator.serviceWorker.register('sw.js', { scope: 'data:text/html,' })
                .then(function () { b.setAttribute('data-scheme', 'resolved'); }, rej('data-scheme'));
              navigator.serviceWorker.register('sw.js', { scope: 'scope%2fx' })
                .then(function () { b.setAttribute('data-encoded', 'resolved'); }, rej('data-encoded'));
              navigator.serviceWorker.getRegistration('http://other.example/')
                .then(function () { b.setAttribute('data-xorigin', 'resolved'); }, rej('data-xorigin'));
            "#
            .to_string()],
            "https://example.com/",
        );
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert_eq!(
            attr_of(&doc, body, "data-scheme").as_deref(),
            Some("TypeError")
        );
        assert_eq!(
            attr_of(&doc, body, "data-encoded").as_deref(),
            Some("TypeError")
        );
        assert_eq!(
            attr_of(&doc, body, "data-xorigin").as_deref(),
            Some("SecurityError")
        );
    }

    #[test]
    fn cache_storage_put_match_keys_delete() {
        // CacheStorage/Cache: open a cache, put a Response, match it back (body intact via clone),
        // enumerate keys, then delete. Exercised on the window global (also exposed in workers).
        let (doc, body) = doc_with_body("");
        let (doc, out) = run_with_dom(
            doc,
            vec![r#"
              var b = document.body;
              caches.open('v1').then(function (c) {
                return c.put(new Request('https://x/a'), new Response('hello'))
                  .then(function () { return c.match('https://x/a'); })
                  .then(function (r) { return r.text(); })
                  .then(function (t) { b.setAttribute('data-match', t); })
                  .then(function () { return c.keys(); })
                  .then(function (keys) { b.setAttribute('data-keys', keys.length + ':' + keys[0].url); })
                  .then(function () { return caches.has('v1'); })
                  .then(function (h) { b.setAttribute('data-has', String(h)); })
                  .then(function () { return c.delete('https://x/a'); })
                  .then(function (d) { return c.match('https://x/a').then(function (m) { b.setAttribute('data-deleted', d + ':' + (m === undefined)); }); });
              });
            "#
            .to_string()],
            "https://example.com/",
        );
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert_eq!(attr_of(&doc, body, "data-match").as_deref(), Some("hello"));
        assert_eq!(
            attr_of(&doc, body, "data-keys").as_deref(),
            Some("1:https://x/a")
        );
        assert_eq!(attr_of(&doc, body, "data-has").as_deref(), Some("true"));
        assert_eq!(
            attr_of(&doc, body, "data-deleted").as_deref(),
            Some("true:true")
        );
    }

    #[test]
    fn message_channel_entangled_ports_deliver_both_ways() {
        // A MessageChannel's two ports are entangled: postMessage on one delivers a `message` event
        // on the other (after the event-loop turn), in both directions. Assigning onmessage starts
        // the port implicitly.
        let (doc, body) = doc_with_body("");
        let (doc, out) = run_with_dom(
            doc,
            vec![r#"
              var b = document.body;
              var mc = new MessageChannel();
              mc.port1.onmessage = function (e) { b.setAttribute('data-on1', e.data); };
              mc.port2.addEventListener('message', function (e) { b.setAttribute('data-on2', e.data); });
              mc.port2.start();
              mc.port2.postMessage('to-1');
              mc.port1.postMessage('to-2');
              b.setAttribute('data-port', String(mc.port1 instanceof MessagePort));
            "#
            .to_string()],
            "https://example.com/",
        );
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert_eq!(attr_of(&doc, body, "data-on1").as_deref(), Some("to-1"));
        assert_eq!(attr_of(&doc, body, "data-on2").as_deref(), Some("to-2"));
        assert_eq!(attr_of(&doc, body, "data-port").as_deref(), Some("true"));
    }

    #[test]
    fn add_event_listener_signal_option_removes_on_abort() {
        let out = env_eval(
            "https://example.com/",
            "var c = new AbortController(); var n = 0; \
             document.addEventListener('ping', function () { n++; }, { signal: c.signal }); \
             document.dispatchEvent(new Event('ping')); c.abort(); document.dispatchEvent(new Event('ping')); \
             String(n)",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("1"));
    }

    #[test]
    fn crypto_get_random_values_fills_nonzero() {
        // Assert the buffer was filled with randomness, not left zeroed. We check that *some* byte
        // is nonzero rather than *every* byte: with a correct RNG any single byte is zero ~1/256 of
        // the time, so `every` over 4 bytes flakes ~1.5% of runs. `some` over 64 bytes only fails
        // if the fill never happened (P(all zero) ≈ (1/256)^64), so it still catches a regression.
        let out = env_eval(
            "https://example.com/",
            "var a = new Uint8Array(64); crypto.getRandomValues(a); \
             a.some(function (x) { return x !== 0; })",
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
    fn font_family_invalid_list_is_rejected_and_escaped_quotes_round_trip() {
        // Regression: a quoted family-name followed by more content (`"times" new roman`) is a
        // syntax error — the declaration must be dropped, not stored as a mangled, unbalanced-quote
        // value. And a *valid* family with an escaped quote (`'\"times new roman'`) must serialize
        // idempotently: re-assigning the serialized form converges instead of growing backslashes
        // on every round-trip. Either bug previously let CSSOM set/serialize loops blow up
        // (unbounded allocation) and hang the engine on css/css-fonts/test_font_family_parsing.html.
        let (doc, _body) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![
                // Invalid: string followed by idents -> whole list dropped, leaving fontFamily empty.
                r#"var a = document.createElement("div"); a.style.fontFamily = "arial, helvetica, \"times\" new roman, sans-serif"; a.style.fontFamily"#.to_string(),
                // Valid escaped quote: assign, read back, re-assign repeatedly; the length must be
                // stable across iterations (idempotent), not grow.
                r#"var b = document.createElement("div"); b.style.fontFamily = 'arial, \'\\"times new roman\', sans-serif'; var lens = []; for (var k = 0; k < 8; k++) { var p = b.style.fontFamily; b.style.fontFamily = "x"; b.style.fontFamily = p; lens.push(b.style.fontFamily.length); } lens.join(",")"#.to_string(),
            ],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some(""),
            "invalid list must be dropped"
        );
        assert_eq!(out[1].error, None, "{:?}", out[1]);
        // All eight reads report the same length -> serialization converged.
        let lens = out[1].value.as_deref().unwrap_or("");
        let stable = lens
            .split(',')
            .collect::<std::collections::HashSet<_>>()
            .len()
            == 1;
        assert!(
            stable,
            "font-family serialization not idempotent: lens = {lens}"
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
    fn resource_timing_buffer_and_observer() {
        // Observing PerformanceObserver({type:"resource"}) must expose recorded resource-timing
        // entries via performance.getEntriesByType, and "resource" must be a supported entry type.
        // (The end-to-end CSS-subresource fetch + CORS-mode behaviour is covered by the
        // css/fetching/fetch-resources WPT reftest; here we assert the synchronous API surface.)
        let out = env_eval(
            "https://example.com/",
            "var po = new PerformanceObserver(function(){}); \
             po.observe({type:'resource', buffered:true}); \
             globalThis.__recordResourceTiming('https://example.com/r.png', 'css'); \
             var e = performance.getEntriesByType('resource'); \
             [e.length, e[0] && e[0].name, e[0] && e[0].entryType, \
              PerformanceObserver.supportedEntryTypes.indexOf('resource') >= 0].join('|')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(
            out.value.as_deref(),
            Some("1|https://example.com/r.png|resource|true")
        );
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
    fn text_and_comment_constructors_create_character_data_nodes() {
        let out = env_eval(
            "https://example.com/",
            r#"
            var textDefault = new Text();
            var textUndefined = new Text(undefined);
            var textNull = new Text(null);
            var commentDefault = new Comment();
            var commentUndefined = new Comment(undefined);
            var commentNumber = new Comment(42);
            [
              textDefault.nodeType,
              textDefault.data,
              textDefault.textContent,
              textDefault.ownerDocument === document,
              textDefault instanceof Text,
              textDefault instanceof CharacterData,
              textUndefined.data,
              textNull.data,
              commentDefault.nodeType,
              commentDefault.data,
              commentDefault.textContent,
              commentDefault.ownerDocument === document,
              commentDefault instanceof Comment,
              commentDefault instanceof CharacterData,
              commentUndefined.data,
              commentNumber.data
            ].join("|")
            "#,
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(
            out.value.as_deref(),
            Some("3|||true|true|true||null|8|||true|true|true||42")
        );
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
    fn node_contains_handles_live_and_disconnected_trees() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var parent = document.createElement("div");
                var child = document.createElement("span");
                parent.appendChild(child);
                document.body.appendChild(parent);
                var attached = [document.contains(document),
                                document.contains(document.documentElement),
                                document.contains(child), parent.contains(parent),
                                parent.contains(child),
                                document.documentElement.parentNode === document];
                parent.remove();
                var detached = [document.contains(parent), document.contains(child),
                                parent.contains(child), child.contains(parent),
                                parent.contains(null)];
                attached.concat(detached).join(",")
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("true,true,true,true,true,true,false,false,true,false,false")
        );
    }

    #[test]
    fn document_contains_is_scoped_to_each_document_tree() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var foreign = document.implementation.createHTMLDocument("");
                var foreignChild = foreign.createElement("p");
                foreign.body.appendChild(foreignChild);
                var xml = document.implementation.createDocument(null, null, null);
                var xmlChild = xml.createElement("item");
                xml.appendChild(xmlChild);
                var detached = foreign.createElement("aside");
                [foreign.contains(foreign), foreign.contains(foreignChild),
                 foreign.contains(document.body), foreign.contains(detached),
                 foreign.contains(foreign.doctype), foreign.doctype.parentNode === foreign,
                 document.contains(foreignChild),
                 xml.contains(xml), xml.contains(xmlChild),
                 xml.contains(foreignChild)].join(",")
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(
            out[0].value.as_deref(),
            Some("true,true,false,false,true,true,false,true,true,false")
        );
    }

    #[test]
    fn child_element_count_counts_only_element_children() {
        // ParentNode.childElementCount must count element children only, ignoring text/comment nodes.
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var d = document.createElement("div");
                    d.appendChild(document.createElement("span"));
                    d.appendChild(document.createTextNode("text"));
                    d.appendChild(document.createElement("b"));
                    d.appendChild(document.createComment("c"));
                    String(d.childElementCount) + "/" + d.childNodes.length"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("2/4"));
    }

    #[test]
    fn node_list_and_html_collection_have_correct_brand_and_liveness() {
        let out = env_eval(
            "https://example.com/",
            r#"
                var host = document.createElement("div");
                var first = document.createElement("span");
                host.appendChild(first);
                var children = host.children;
                var childNodes = host.childNodes;
                var tags = host.getElementsByTagName("span");
                var snapshot = host.querySelectorAll("span");
                host.appendChild(document.createTextNode("x"));
                host.appendChild(document.createElement("span"));
                [Object.prototype.toString.call(children),
                 Object.prototype.toString.call(childNodes),
                 children instanceof HTMLCollection, childNodes instanceof NodeList,
                 host.children === children, host.childNodes === childNodes,
                 children.length, childNodes.length, tags.length, snapshot.length,
                 snapshot.item(0) === first].join("|")
            "#,
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(
            out.value.as_deref(),
            Some("[object HTMLCollection]|[object NodeList]|true|true|true|true|2|3|2|1|true")
        );
    }

    #[test]
    fn dom_collections_support_iteration_named_access_and_expandos() {
        let out = env_eval(
            "https://example.com/",
            r#"
                var host = document.createElement("div");
                var first = document.createElement("span"); first.id = "first";
                var second = document.createElement("span"); second.setAttribute("name", "second");
                host.append(first, second);
                var collection = host.getElementsByTagName("span");
                var list = host.querySelectorAll("span");
                var iterated = []; for (var node of list) { iterated.push(node.id || node.getAttribute("name")); }
                var strictIndexWriteThrows = false;
                try { (function () { "use strict"; collection[5] = first; })(); }
                catch (e) { strictIndexWriteThrows = e instanceof TypeError; }
                var stringItemIsFirst = collection.item("not-a-number") === first;
                collection.item = "expando";
                Object.defineProperty(list, "length", { get: function () { return 1; } });
                [collection.namedItem("first") === first, collection.first === first,
                 collection.second === second, collection[0] === first,
                 stringItemIsFirst, first instanceof Element,
                 collection.item, strictIndexWriteThrows,
                 iterated.join(","), list.length, list[1] === second].join("|")
            "#,
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(
            out.value.as_deref(),
            Some("true|true|true|true|true|true|expando|true|first,second|1|true")
        );
    }

    #[test]
    fn html_collection_item_converts_nonnumeric_index_for_parsed_elements() {
        let doc = html::parse(
            r#"<!doctype html><html><head></head><body><div id="host"><img><img id="foo"></div></body></html>"#,
        );
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"
                var collection = document.getElementById("host").children;
                var item = collection.item("foo");
                [item === collection[0], item instanceof Element, item && item.tagName].join("|")
            "#
            .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("true|true|IMG"));
    }

    #[test]
    fn document_named_and_legacy_collections_are_live() {
        let out = env_eval(
            "https://example.com/",
            r#"
                var named = document.getElementsByName("target");
                var scripts = document.scripts;
                var links = document.links;
                var input = document.createElement("input"); input.setAttribute("name", "target");
                var script = document.createElement("script");
                var link = document.createElement("a"); link.href = "/x";
                document.body.append(input, script, link);
                var afterAppend = [named.length, scripts.length, links.length];
                input.remove(); script.remove(); link.remove();
                [Object.prototype.toString.call(named),
                 Object.prototype.toString.call(scripts),
                 named instanceof NodeList, scripts instanceof HTMLCollection,
                 afterAppend.join(","), named.length, scripts.length, links.length].join("|")
            "#,
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(
            out.value.as_deref(),
            Some("[object NodeList]|[object HTMLCollection]|true|true|1,1,1|0|0|0")
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
    fn service_worker_register_runs_lifecycle_to_activated() {
        // With a fetcher that serves the worker script as JS, register() resolves with a
        // ServiceWorkerRegistration whose worker starts in "installing" and is advanced through the
        // lifecycle to "activated" (becoming the registration's active worker) during the init
        // drain. The registration's navigationPreload also round-trips a header value.
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                navigator.serviceWorker.register('sw.js').then(function (reg) {
                  var w = reg.installing;
                  document.body.setAttribute('data-initial', w ? w.state : 'none');
                  w.addEventListener('statechange', function () {
                    if (w.state === 'activated') {
                      document.body.setAttribute('data-final', reg.active === w ? 'activated' : 'mismatch');
                    }
                  });
                  return reg.navigationPreload.setHeaderValue('hello')
                    .then(function () { return reg.navigationPreload.getState(); })
                    .then(function (s) { document.body.setAttribute('data-hdr', s.headerValue); });
                }, function (e) { document.body.setAttribute('data-err', (e && e.name) || String(e)); });
            "#
            .to_string(),
        );
        // Serve any requested URL as a JavaScript resource (echoing the URL into the envelope).
        let request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync> =
            Arc::new(|_m, u, _b, _h| {
                Some(format!(
                    r#"{{"ok":true,"status":200,"statusText":"OK","url":"{u}","contentType":"text/javascript","body":"// worker"}}"#
                ))
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
        let attr = |name: &str| match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get(name).cloned(),
            _ => None,
        };
        assert_eq!(attr("data-err"), None, "register should not reject");
        assert_eq!(attr("data-initial").as_deref(), Some("installing"));
        assert_eq!(attr("data-final").as_deref(), Some("activated"));
        assert_eq!(attr("data-hdr").as_deref(), Some("hello"));
    }

    #[test]
    fn service_worker_runs_script_and_round_trips_messages() {
        // The worker script executes in a ServiceWorkerGlobalScope: it can call skipWaiting() and
        // register a `message` listener that replies through event.source (the page client). After
        // activation the page posts a message to registration.active and receives the worker's reply
        // on navigator.serviceWorker, exercising the full page<->worker messaging bridge.
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                navigator.serviceWorker.onmessage = function (e) {
                  document.body.setAttribute('data-reply', e.data);
                };
                navigator.serviceWorker.register('sw.js').then(function (reg) {
                  var w = reg.installing;
                  w.addEventListener('statechange', function () {
                    if (w.state === 'activated') { reg.active.postMessage('ping'); }
                  });
                });
            "#
            .to_string(),
        );
        // The worker script (JSON-escaped into the envelope body): reply via event.source.postMessage.
        let worker_body = "self.skipWaiting();\\nself.addEventListener('message', function (e) { e.source.postMessage('pong:' + e.data); });";
        let request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync> =
            Arc::new(move |_m, u, _b, _h| {
                Some(format!(
                    r#"{{"ok":true,"status":200,"statusText":"OK","url":"{u}","contentType":"text/javascript","body":"{worker_body}"}}"#
                ))
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
        let reply = match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get("data-reply").cloned(),
            _ => None,
        };
        assert_eq!(reply.as_deref(), Some("pong:ping"));
    }

    #[test]
    fn dedicated_worker_round_trips_messages_and_imports() {
        // A dedicated Worker runs its script in a DedicatedWorkerGlobalScope on the page's shared
        // event loop. The page posts a message; the worker replies via self.postMessage; the reply
        // arrives on worker.onmessage. Exercises construction, synchronous script fetch,
        // importScripts, `with (self)` bare-global scoping (the worker calls bare `tag(...)` and
        // bare `postMessage(...)`), and bidirectional structured-clone messaging.
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                var w = new Worker('worker.js');
                w.onmessage = function (e) { document.body.setAttribute('data-reply', e.data); };
                w.postMessage('ping');
            "#
            .to_string(),
        );
        let request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync> =
            Arc::new(|_m, u, _b, _h| {
                let body = if u.ends_with("helper.js") {
                    "self.tag = function (s) { return 'pong:' + s; };"
                } else if u.ends_with("worker.js") {
                    "importScripts('helper.js'); self.onmessage = function (e) { postMessage(tag(e.data)); };"
                } else {
                    ""
                };
                Some(format!(
                    r#"{{"ok":true,"status":200,"statusText":"OK","url":"{u}","contentType":"text/javascript","body":"{body}"}}"#
                ))
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
        let reply = match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get("data-reply").cloned(),
            _ => None,
        };
        assert_eq!(reply.as_deref(), Some("pong:ping"));
    }

    #[test]
    fn dedicated_worker_importscripts_shares_top_level_declarations() {
        // The realm fix: each worker is its own V8 context, so `self === globalThis` and top-level
        // `function`/`var` declarations in an importScripts'd file become worker globals visible to
        // later scripts — exactly what canvas-tests.js et al. rely on (bare `function _assertSame`).
        // Here helper.js declares a bare `function helperFn` and a bare `var helperVar`; the worker
        // reports both back, proving cross-file global sharing inside the worker realm.
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                var w = new Worker('worker.js');
                w.onmessage = function (e) { document.body.setAttribute('data-reply', e.data); };
            "#
            .to_string(),
        );
        let request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync> =
            Arc::new(|_m, u, _b, _h| {
                let body = if u.ends_with("helper.js") {
                    // Bare top-level declarations (no `self.` / `var x =` on globalThis).
                    "function helperFn(s) { return 'fn:' + s; } var helperVar = 'V';"
                } else if u.ends_with("worker.js") {
                    "importScripts('helper.js'); postMessage(helperFn(helperVar));"
                } else {
                    ""
                };
                Some(format!(
                    r#"{{"ok":true,"status":200,"statusText":"OK","url":"{u}","contentType":"text/javascript","body":"{body}"}}"#
                ))
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
        let reply = match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get("data-reply").cloned(),
            _ => None,
        };
        assert_eq!(reply.as_deref(), Some("fn:V"));
    }

    #[test]
    fn url_webidl_conformance() {
        // URL/URLSearchParams are proper WebIDL interfaces: members on the prototype, @@toStringTag,
        // constructors throw without `new`, and methods enforce required arguments.
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                var out = [];
                out.push(Object.getOwnPropertyDescriptor(URL.prototype, "href") != null); // on prototype
                out.push(Object.prototype.toString.call(new URL("http://h/")) === "[object URL]");
                out.push(URL.length === 1);
                try { URL("http://h/"); out.push(false); } catch (e) { out.push(e instanceof TypeError); }
                out.push(Object.getOwnPropertyDescriptor(URLSearchParams.prototype, "append") != null);
                try { URLSearchParams(); out.push(false); } catch (e) { out.push(e instanceof TypeError); }
                var p = new URLSearchParams("a=1&b=2");
                try { p.get(); out.push(false); } catch (e) { out.push(e instanceof TypeError); }
                out.push(p.size === 2 && p[Symbol.iterator] === p.entries);
                document.body.setAttribute('data-out', out.join(","));
            "#
            .to_string(),
        );
        let (_session, snapshot, out) = Session::new(
            doc,
            vec![],
            vec![entry],
            modules,
            "https://x/",
            no_fetch(),
            no_request(),
            no_ws(),
            None,
        );
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        let attr = match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get("data-out").cloned(),
            _ => None,
        };
        assert_eq!(
            attr.as_deref(),
            Some("true,true,true,true,true,true,true,true")
        );
    }

    #[test]
    fn window_open_throws_on_invalid_url() {
        // window.open() with an unparseable URL throws a SyntaxError DOMException; a valid URL
        // returns a window.
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                var bad = '';
                try { self.open('file://example:1/'); } catch (e) { bad = e.name; }
                document.body.setAttribute('data-bad', bad);
                document.body.setAttribute('data-good', String(self.open('https://ok.example/') != null));
            "#
            .to_string(),
        );
        let (_session, snapshot, out) = Session::new(
            doc,
            vec![],
            vec![entry],
            modules,
            "https://x/",
            no_fetch(),
            no_request(),
            no_ws(),
            None,
        );
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        let attr = |name: &str| match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get(name).cloned(),
            _ => None,
        };
        assert_eq!(
            attr("data-bad").as_deref(),
            Some("SyntaxError"),
            "invalid URL throws SyntaxError"
        );
        assert_eq!(
            attr("data-good").as_deref(),
            Some("true"),
            "valid URL returns a window"
        );
    }

    #[test]
    fn urlsearchparams_usvstring_record_init() {
        // Object init builds a record<USVString,USVString>: lone surrogates -> U+FFFD, and coerced
        // keys collapse (later value wins, first position kept).
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                var a = new URLSearchParams({ "x\uDC53": "1", "x\uDC5C": "2", "x\uDC65": "3" });
                var b = new URLSearchParams({ "\uD835x": "1", "xx": "2", "\uD83Dx": "3" });
                document.body.setAttribute('data-a', a.toString());
                document.body.setAttribute('data-b', b.toString());
            "#
            .to_string(),
        );
        let (_session, snapshot, out) = Session::new(
            doc,
            vec![],
            vec![entry],
            modules,
            "https://x/",
            no_fetch(),
            no_request(),
            no_ws(),
            None,
        );
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        let attr = |name: &str| match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get(name).cloned(),
            _ => None,
        };
        // U+FFFD encodes to %EF%BF%BD in application/x-www-form-urlencoded.
        assert_eq!(
            attr("data-a").as_deref(),
            Some("x%EF%BF%BD=3"),
            "3 keys collapse to one, last value"
        );
        assert_eq!(
            attr("data-b").as_deref(),
            Some("%EF%BF%BDx=3&xx=2"),
            "first position kept, value updated"
        );
    }

    #[test]
    fn anchor_click_executes_javascript_url() {
        // Clicking <a href="javascript:..."> runs the script in the page realm; a javascript: URL
        // that fails to parse (invalid host) does not execute.
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                var a = document.body.appendChild(document.createElement('a'));
                a.href = 'javascript:globalThis.ranGood = 7';
                a.click();
                a.href = 'javascript://test:test/%0aglobalThis.ranBad = 1';
                a.click();
                // javascript: URLs run in a queued task, so read after them.
                setTimeout(function () {
                  document.body.setAttribute('data-good', String(globalThis.ranGood));
                  document.body.setAttribute('data-bad', String(globalThis.ranBad));
                }, 0);
            "#
            .to_string(),
        );
        let (_session, snapshot, out) = Session::new(
            doc,
            vec![],
            vec![entry],
            modules,
            "https://x/",
            no_fetch(),
            no_request(),
            no_ws(),
            None,
        );
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        let attr = |name: &str| match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get(name).cloned(),
            _ => None,
        };
        assert_eq!(
            attr("data-good").as_deref(),
            Some("7"),
            "valid javascript: URL executes"
        );
        assert_eq!(
            attr("data-bad").as_deref(),
            Some("undefined"),
            "invalid-host javascript: URL does not execute"
        );
    }

    #[test]
    fn srcless_iframe_has_window_and_location_throws_on_invalid() {
        // A srcless <iframe> still has an about:blank browsing context: contentWindow exposes the
        // frame realm's globals (e.g. DOMException), and assigning contentWindow.location an invalid
        // URL throws the frame's SyntaxError DOMException (PutForwards=href).
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                var f = document.body.appendChild(document.createElement('iframe'));
                document.body.setAttribute('data-de', typeof f.contentWindow.DOMException);
                var threw = '';
                try { f.contentWindow.location = 'file://example:1/'; }
                catch (e) { threw = e.name + ':' + (e instanceof f.contentWindow.DOMException); }
                document.body.setAttribute('data-throw', threw);
            "#
            .to_string(),
        );
        let (_session, snapshot, out) = Session::new(
            doc,
            vec![],
            vec![entry],
            modules,
            "https://x/",
            no_fetch(),
            no_request(),
            no_ws(),
            None,
        );
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        let attr = |name: &str| match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get(name).cloned(),
            _ => None,
        };
        assert_eq!(
            attr("data-de").as_deref(),
            Some("function"),
            "frame has DOMException"
        );
        assert_eq!(
            attr("data-throw").as_deref(),
            Some("SyntaxError:true"),
            "invalid location throws frame's SyntaxError"
        );
    }

    #[test]
    fn url_host_setter_conformance() {
        // host setter: a file URL can't have a port, so `host = 'x:123'` is rejected (no-op); a
        // special URL's host setter splits host:port; an opaque-path query removal keeps trailing
        // spaces (encoding the final one).
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                var b = document.body;
                var f = new URL('file://y/'); f.host = 'x:123';
                b.setAttribute('data-file', f.href);
                var h = new URL('http://y/'); h.host = 'z:8080';
                b.setAttribute('data-http', h.host + '|' + h.href);
                var u = new URL('data:space    ?test'); u.search = '';
                b.setAttribute('data-opaque', u.href);
            "#
            .to_string(),
        );
        let (_session, snapshot, out) = Session::new(
            doc,
            vec![],
            vec![entry],
            modules,
            "https://x/",
            no_fetch(),
            no_request(),
            no_ws(),
            None,
        );
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        let attr = |name: &str| match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get(name).cloned(),
            _ => None,
        };
        assert_eq!(
            attr("data-file").as_deref(),
            Some("file://y/"),
            "file host with port is rejected"
        );
        assert_eq!(
            attr("data-http").as_deref(),
            Some("z:8080|http://z:8080/"),
            "host splits host:port"
        );
        assert_eq!(
            attr("data-opaque").as_deref(),
            Some("data:space   %20"),
            "opaque trailing space kept"
        );
    }

    #[test]
    fn diag_sethost_panic() {
        // file URLs + various host values; find which set_host panics (url crate bug).
        let bases = ["file://y/", "file:///p", "http://h/", "sc://x/"];
        let vals = [
            "", "a", "a:1", "[::1]", "x|", "C:", "/", "\\", "h\\p", "a b", "a%2fb", "host:", ":80",
            "[v1.x]",
        ];
        for b in bases {
            for v in vals {
                let r = std::panic::catch_unwind(|| {
                    let mut u = url::Url::parse(b).unwrap();
                    let _ = u.set_host(Some(v));
                    u.as_str().to_string()
                });
                if r.is_err() {
                    eprintln!("DIAG PANIC base={:?} host={:?}", b, v);
                }
            }
        }
        eprintln!("DIAG done");
    }

    #[test]
    fn url_searchparams_conformance() {
        // URLSearchParams/URL conformance: set() updates the first occurrence in place + removes the
        // rest; url.search setter clears searchParams; lenient form percent-decode keeps invalid `%`
        // literal but decodes valid escapes; URL.parse(relative, opaque-base) is null.
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                var b = document.body;
                var p = new URLSearchParams('a=b&c=d&a=e'); p.set('a', 'B');
                b.setAttribute('data-set', p.toString());
                var u = new URL('http://h/?a=1&b=2&a=3'); u.search = '?';
                b.setAttribute('data-clear', String(u.searchParams.size) + '|' + u.href);
                b.setAttribute('data-dec', new URLSearchParams('b=%2sf%2a').toString());
                b.setAttribute('data-parse', String(URL.parse('undefined', 'aaa:b')));
                b.setAttribute('data-static', typeof URL.canParse + ',' + typeof URL.parse);
            "#
            .to_string(),
        );
        let (_session, snapshot, out) = Session::new(
            doc,
            vec![],
            vec![entry],
            modules,
            "https://x/",
            no_fetch(),
            no_request(),
            no_ws(),
            None,
        );
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        let attr = |name: &str| match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get(name).cloned(),
            _ => None,
        };
        assert_eq!(
            attr("data-set").as_deref(),
            Some("a=B&c=d"),
            "set updates first, removes rest"
        );
        assert_eq!(
            attr("data-clear").as_deref(),
            Some("0|http://h/"),
            "url.search='?' clears query"
        );
        assert_eq!(
            attr("data-dec").as_deref(),
            Some("b=%252sf*"),
            "lenient form decode"
        );
        assert_eq!(
            attr("data-parse").as_deref(),
            Some("null"),
            "relative vs opaque base is null"
        );
        assert_eq!(
            attr("data-static").as_deref(),
            Some("function,function"),
            "URL.canParse/parse exist"
        );
    }

    #[test]
    fn iframe_loads_runs_scripts_and_cross_frame_postmessage() {
        // An <iframe src> loads as a real nested realm: its script runs, the element fires `load`,
        // and cross-frame postMessage works both ways. The frame's script posts to parent on a
        // message; the page posts to the frame and records the reply.
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                var f = document.createElement('iframe');
                f.onload = function () { document.body.setAttribute('data-loaded', 'yes'); };
                window.addEventListener('message', function (e) { document.body.setAttribute('data-reply', e.data); });
                f.src = 'frame.html';
                document.body.appendChild(f);
                f.onload = function () {
                  document.body.setAttribute('data-loaded', 'yes');
                  f.contentWindow.postMessage('hi', '*');
                };
            "#
            .to_string(),
        );
        let request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync> =
            Arc::new(|_m, u, _b, _h| {
                if u.ends_with("frame.html") {
                    let body = "<!doctype html><html><body><script>self.addEventListener('message', function (e) { parent.postMessage('echo:' + e.data, '*'); });<\\/script></body></html>";
                    Some(format!(
                        r#"{{"ok":true,"status":200,"statusText":"OK","url":"{u}","contentType":"text/html","body":"{body}"}}"#
                    ))
                } else {
                    None
                }
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
        let attr = |name: &str| match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get(name).cloned(),
            _ => None,
        };
        assert_eq!(
            attr("data-loaded").as_deref(),
            Some("yes"),
            "iframe should fire load"
        );
        assert_eq!(
            attr("data-reply").as_deref(),
            Some("echo:hi"),
            "cross-frame postMessage round-trip"
        );
    }

    #[test]
    fn dedicated_worker_async_fetch_resolves() {
        // A worker's async fetch() resolves: its completion is routed back into the worker context by
        // pump_workers (its own fetch channel), so the promise settles and the worker posts the body.
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                var w = new Worker('worker.js');
                w.onmessage = function (e) { document.body.setAttribute('data-reply', e.data); };
            "#
            .to_string(),
        );
        let request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync> =
            Arc::new(|_m, u, _b, _h| {
                let body = if u.ends_with("worker.js") {
                    "fetch('data.txt').then(function (r) { return r.text(); }).then(function (t) { postMessage('got:' + t); });".to_string()
                } else if u.ends_with("data.txt") {
                    r#"{"ok":true,"status":200,"statusText":"OK","url":"x","contentType":"text/plain","body":"HELLO"}"#.to_string()
                } else {
                    return None;
                };
                if u.ends_with("data.txt") {
                    Some(body)
                } else {
                    Some(format!(
                        r#"{{"ok":true,"status":200,"statusText":"OK","url":"{u}","contentType":"text/javascript","body":"{body}"}}"#
                    ))
                }
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
        let reply = match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get("data-reply").cloned(),
            _ => None,
        };
        assert_eq!(reply.as_deref(), Some("got:HELLO"));
    }

    #[test]
    fn dedicated_worker_from_blob_object_url() {
        // `new Worker(URL.createObjectURL(new Blob([src])))` — an inline (blob/data:) worker. The
        // page decodes the data: URL and hands the source to the worker context directly. The worker
        // reports back its performance.timeOrigin type, proving it ran with a working environment.
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                var src = "postMessage('to:' + (typeof performance.timeOrigin));";
                var w = new Worker(URL.createObjectURL(new Blob([src], { type: 'text/javascript' })));
                w.onmessage = function (e) { document.body.setAttribute('data-reply', e.data); };
            "#
            .to_string(),
        );
        let (_session, snapshot, out) = Session::new(
            doc,
            vec![],
            vec![entry],
            modules,
            "https://x/",
            no_fetch(),
            no_request(),
            no_ws(),
            None,
        );
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        let reply = match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get("data-reply").cloned(),
            _ => None,
        };
        assert_eq!(reply.as_deref(), Some("to:number"));
    }

    #[test]
    fn cross_origin_isolated_reflects_engine_flag() {
        // The engine sets the COOP+COEP isolation flag from the main document's response; the JS env
        // reflects it as self.crossOriginIsolated (and a worker inherits the page's value).
        set_cross_origin_isolated(true);
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                var w = new Worker('worker.js');
                w.onmessage = function (e) { document.body.setAttribute('data-worker', e.data); };
                document.body.setAttribute('data-page', String(self.crossOriginIsolated));
            "#
            .to_string(),
        );
        let request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync> =
            Arc::new(|_m, u, _b, _h| {
                let body = if u.ends_with("worker.js") {
                    "postMessage('w:' + self.crossOriginIsolated)"
                } else {
                    ""
                };
                Some(format!(
                    r#"{{"ok":true,"status":200,"statusText":"OK","url":"{u}","contentType":"text/javascript","body":"{body}"}}"#
                ))
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
        set_cross_origin_isolated(false); // reset the process-global flag for other tests
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        let attr = |name: &str| match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get(name).cloned(),
            _ => None,
        };
        assert_eq!(attr("data-page").as_deref(), Some("true"));
        assert_eq!(attr("data-worker").as_deref(), Some("w:true"));
    }

    #[test]
    fn performance_timeorigin_now_and_crossoriginisolated() {
        // hr-time: performance.now() is a positive number, timeOrigin is a real epoch close to
        // Date.now(), toJSON().timeOrigin matches, and crossOriginIsolated is a boolean.
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                var b = document.body;
                b.setAttribute('data-now-pos', String(performance.now() > 0));
                b.setAttribute('data-origin-close', String(Math.abs(Date.now() - performance.timeOrigin) < 1000));
                b.setAttribute('data-tojson', String(performance.toJSON().timeOrigin === performance.timeOrigin));
                b.setAttribute('data-coi', typeof self.crossOriginIsolated);
                var n1 = performance.now(), n2 = performance.now();
                b.setAttribute('data-monotonic', String((n2 - n1) >= 0));
            "#
            .to_string(),
        );
        let (_session, snapshot, out) = Session::new(
            doc,
            vec![],
            vec![entry],
            modules,
            "https://x/",
            no_fetch(),
            no_request(),
            no_ws(),
            None,
        );
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        let attr = |name: &str| match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get(name).cloned(),
            _ => None,
        };
        assert_eq!(
            attr("data-now-pos").as_deref(),
            Some("true"),
            "now() should be positive"
        );
        assert_eq!(
            attr("data-origin-close").as_deref(),
            Some("true"),
            "timeOrigin close to Date.now()"
        );
        assert_eq!(
            attr("data-tojson").as_deref(),
            Some("true"),
            "toJSON().timeOrigin matches"
        );
        assert_eq!(
            attr("data-coi").as_deref(),
            Some("boolean"),
            "crossOriginIsolated is boolean"
        );
        assert_eq!(
            attr("data-monotonic").as_deref(),
            Some("true"),
            "now() monotonic"
        );
    }

    #[test]
    fn offscreen_canvas_2d_reads_back_drawn_pixels() {
        // An OffscreenCanvas 2D context records a display list; getImageData rasterizes it
        // synchronously via the paint-crate rasterizer (the __rasterizeCanvas native), so a filled
        // rect reads back as the fill color — no engine compositing pass needed.
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                var oc = new OffscreenCanvas(10, 10);
                var ctx = oc.getContext('2d');
                ctx.fillStyle = '#ff0000';
                ctx.fillRect(0, 0, 10, 10);
                var d = ctx.getImageData(2, 2, 1, 1).data;
                document.body.setAttribute('data-px', d[0] + ',' + d[1] + ',' + d[2] + ',' + d[3]);
            "#
            .to_string(),
        );
        let (_session, snapshot, out) = Session::new(
            doc,
            vec![],
            vec![entry],
            modules,
            "https://x/",
            no_fetch(),
            no_request(),
            no_ws(),
            None,
        );
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        let px = match &snapshot.get(body).data {
            dom::NodeData::Element(e) => e.attrs.get("data-px").cloned(),
            _ => None,
        };
        assert_eq!(px.as_deref(), Some("255,0,0,255"));
    }

    #[test]
    fn service_worker_intercepts_controlled_fetch() {
        // After the worker activates and clients.claim()s the page, a fetch() from the page is
        // dispatched to the worker's `fetch` handler, which respondWith()s a synthetic Response.
        let (doc, body) = doc_with_body("");
        let entry = "https://x/app.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            entry.clone(),
            r#"
                navigator.serviceWorker.addEventListener('controllerchange', function () {
                  fetch('https://x/magic').then(function (r) { return r.text(); })
                    .then(function (t) { document.body.setAttribute('data-fetch', t); });
                });
                navigator.serviceWorker.register('sw.js', { scope: '/' });
            "#
            .to_string(),
        );
        let worker_body = "self.addEventListener('install', function (e) { self.skipWaiting(); });\\nself.addEventListener('activate', function (e) { e.waitUntil(self.clients.claim()); });\\nself.addEventListener('fetch', function (e) { if (e.request.url.indexOf('magic') >= 0) { e.respondWith(new Response('intercepted')); } });";
        let request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync> =
            Arc::new(move |_m, u, _b, _h| {
                // Serve the worker script as JS; anything else (shouldn't be hit) as a marker.
                let body = if u.ends_with("sw.js") {
                    worker_body
                } else {
                    "NETWORK"
                };
                Some(format!(
                    r#"{{"ok":true,"status":200,"statusText":"OK","url":"{u}","contentType":"text/javascript","body":"{body}"}}"#
                ))
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
            dom::NodeData::Element(e) => e.attrs.get("data-fetch").cloned(),
            _ => None,
        };
        assert_eq!(got.as_deref(), Some("intercepted"));
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
