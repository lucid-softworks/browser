//! Iframe browsing contexts as real, separate V8 *contexts* in the page's isolate.
//!
//! An `<iframe>` with a `src`/`srcdoc` gets its own `v8::Context` (like a worker realm) holding a
//! freshly parsed document and running that document's scripts — so `self === globalThis` is the
//! frame's own window, the frame has its own `document`/`location`/`performance`, and its scripts
//! run in isolation. The existing event-loop drain pumps each frame cooperatively (same isolate,
//! same thread; no real parallelism). Cross-frame `postMessage` bridges contexts via the receiver's
//! own structuredClone, and `window.parent`/`frameElement` are wired through the bridge natives.
//!
//! Reuses the realm pattern from [`crate::worker`]: a frame is essentially a mini page-session that
//! shares the page's isolate and event loop. The page context is registered via
//! [`crate::set_page_context`] (shared with workers); [`pump_frames`] is called from
//! [`crate::drain_event_loop`] to advance each frame's timers / fetches / message deliveries.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Receiver;

use crate::{
    deliver_fetch_completion, deliver_ws_event, eval_internal, eval_source, host_state,
    install_browser_environment, run_due_timers, FetchCompletion, HostState, SharedDoc, WsEvent,
};
use crate::primitives::{arg_str, set_fn};

/// JS overlay run in the frame context after the browser env is installed: wires `parent`/`top`/
/// `frameElement`/`postMessage`-to-parent and fires the frame's own load lifecycle.
const FRAME_ENV_BOOTSTRAP: &str = include_str!("bootstrap/frame_env.js");

/// One live iframe browsing context, keyed by the host `<iframe>` element's node id (page side).
struct FrameRealm {
    node_id: u32,
    context: v8::Global<v8::Context>,
    fetch_rx: Receiver<FetchCompletion>,
    ws_evt_rx: Receiver<WsEvent>,
    alive: bool,
}

thread_local! {
    static FRAME_REG: RefCell<Vec<FrameRealm>> = const { RefCell::new(Vec::new()) };
}

/// Drop all frame realms (called on session teardown, before the isolate).
pub fn clear_frames() {
    FRAME_REG.with(|r| r.borrow_mut().clear());
}

/// Install the iframe bridge natives onto a context's global (page + frame contexts, so nested
/// iframes work).
pub(crate) fn register_iframe_natives(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    set_fn(scope, global, "__iframeLoad", prim_iframe_load);
    set_fn(scope, global, "__framePostToFrame", prim_frame_post_to_frame);
    set_fn(scope, global, "__framePostToParent", prim_frame_post_to_parent);
    set_fn(scope, global, "__frameUnload", prim_frame_unload);
    set_fn(scope, global, "__frameGet", prim_frame_get);
}

/// `__frameGet(nodeId, prop)` -> the value of `globalThis[prop]` in the frame's context (or
/// undefined). Used by the page's `contentWindow` proxy so `frame.contentWindow.performance` /
/// `.location` / `.document` reach the frame realm's real objects (same isolate → the handle is
/// valid in the page context).
fn prim_frame_get(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let node_id = args.get(0).number_value(scope).unwrap_or(0.0) as u32;
    let prop = arg_str(scope, &args, 1);
    let ctx_g = FRAME_REG.with(|r| {
        r.borrow()
            .iter()
            .find(|f| f.node_id == node_id && f.alive)
            .map(|f| f.context.clone())
    });
    if let Some(ctx_g) = ctx_g {
        let ctx = v8::Local::new(scope, &ctx_g);
        let cscope = &mut v8::ContextScope::new(scope, ctx);
        let global = cscope.get_current_context().global(cscope);
        if let Some(key) = v8::String::new(cscope, &prop) {
            if let Some(v) = global.get(cscope, key.into()) {
                rv.set(v);
            }
        }
    }
}

/// Walk a parsed document for runnable classic `<script>`s in document order: `(is_external, value)`
/// where value is the absolute URL (external) or the inline body. Mirrors the engine's
/// `collect_script_sources` (the js crate can't depend on `engine`). `<script type=module>` and
/// non-JS types are skipped (modules in frames are best-effort / not run here).
fn collect_classic_scripts(doc: &dom::Document, base: &str) -> Vec<(bool, String)> {
    fn is_js_type(ty: Option<&str>) -> bool {
        match ty {
            None => true,
            Some(t) => {
                let t = t.trim().to_ascii_lowercase();
                matches!(
                    t.as_str(),
                    "" | "text/javascript"
                        | "application/javascript"
                        | "text/ecmascript"
                        | "application/ecmascript"
                )
            }
        }
    }
    fn walk(doc: &dom::Document, id: dom::NodeId, base: &str, out: &mut Vec<(bool, String)>) {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag == "script" {
                // type=module is not classic; skip (best-effort — frames rarely need modules here).
                let ty = e.attrs.get("type").map(String::as_str);
                if ty.map(|t| t.trim().eq_ignore_ascii_case("module")) != Some(true) && is_js_type(ty)
                {
                    if let Some(src) = e.attrs.get("src") {
                        if let Ok(abs) = url::Url::parse(base).and_then(|u| u.join(src)) {
                            out.push((true, abs.to_string()));
                        } else {
                            out.push((true, src.clone()));
                        }
                    } else {
                        let mut body = String::new();
                        for &child in &doc.get(id).children {
                            if let dom::NodeData::Text(t) = &doc.get(child).data {
                                body.push_str(t);
                            }
                        }
                        out.push((false, body));
                    }
                }
                return; // never descend into a script's text body
            }
        }
        for &child in &doc.get(id).children {
            walk(doc, child, base, out);
        }
    }
    let mut out = Vec::new();
    walk(doc, doc.root(), base, &mut out);
    out
}

/// `__iframeLoad(nodeId, url, srcdoc) -> bool`. Build the frame's context from `srcdoc` (inline HTML)
/// or by fetching `url`, run its classic scripts, and register the realm. Returns whether a document
/// was loaded (false → the caller fires an `error`; true → it fires `load`).
fn prim_iframe_load(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let node_id = args.get(0).number_value(scope).unwrap_or(0.0) as u32;
    let url = arg_str(scope, &args, 1);
    let srcdoc: Option<String> = {
        let a = args.get(2);
        if a.is_string() {
            Some(a.to_rust_string_lossy(scope))
        } else {
            None
        }
    };

    let page_state = host_state(scope);
    let request_fetcher = std::sync::Arc::clone(&page_state.request_fetcher);
    let ws_connector = std::sync::Arc::clone(&page_state.ws_connector);

    // Resolve the frame's document HTML + its base URL.
    let (html_src, frame_url) = match &srcdoc {
        Some(s) => (s.clone(), url.clone()),
        None => {
            if url.is_empty() || url == "about:blank" {
                (String::new(), "about:blank".to_string())
            } else {
                // Fetch the document HTML via the host fetcher (envelope JSON: {ok,status,body,...}).
                match request_fetcher("GET", &url, "", "{}") {
                    Some(env) => match extract_envelope_body(&env) {
                        Some(body) => (body, url.clone()),
                        None => {
                            rv.set(v8::Boolean::new(scope, false).into());
                            return;
                        }
                    },
                    None => {
                        rv.set(v8::Boolean::new(scope, false).into());
                        return;
                    }
                }
            }
        }
    };

    let doc = if html_src.is_empty() {
        dom::Document::new()
    } else {
        html::parse(&html_src)
    };
    let scripts = collect_classic_scripts(&doc, &frame_url);

    let (fetch_tx, fetch_rx) = std::sync::mpsc::channel();
    let (ws_tx, ws_evt_rx) = std::sync::mpsc::channel();

    let ctx_global = {
        let ctx = v8::Context::new(scope, Default::default());
        let cscope = &mut v8::ContextScope::new(scope, ctx);
        let shared: SharedDoc = Rc::new(RefCell::new(doc));
        let state = HostState::with_fetcher(
            Rc::clone(&shared),
            Rc::new(|_| None),
            request_fetcher.clone(),
            fetch_tx,
            ws_connector,
            ws_tx,
        );
        cscope.get_current_context().set_slot(state);
        install_browser_environment(cscope, &frame_url);
        // Seed the host node id so the overlay can wire frameElement/parent messaging.
        let g = cscope.get_current_context().global(cscope);
        let kid = v8::String::new(cscope, "__frameNodeId").unwrap();
        let vid = v8::Number::new(cscope, node_id as f64);
        g.set(cscope, kid.into(), vid.into());
        eval_internal(cscope, FRAME_ENV_BOOTSTRAP, "<frame-env>");

        // Run the document's classic scripts in order (external ones fetched synchronously).
        for (is_external, value) in scripts {
            if is_external {
                if let Some(env) = request_fetcher("GET", &value, "", "{}") {
                    if let Some(body) = extract_envelope_body(&env) {
                        eval_source(cscope, &body, &value);
                    }
                }
            } else {
                eval_source(cscope, &value, &frame_url);
            }
        }
        // Fire the frame's own DOMContentLoaded/load lifecycle.
        eval_internal(
            cscope,
            "if (typeof __fireLifecycleEvents === 'function') { __fireLifecycleEvents(); } \
             if (typeof __enterRealtime === 'function') { __enterRealtime(); }",
            "<frame-lifecycle>",
        );
        v8::Global::new(cscope, ctx)
    };

    FRAME_REG.with(|r| {
        // Replace any prior realm for this node (re-navigation).
        r.borrow_mut().retain(|f| f.node_id != node_id);
        r.borrow_mut().push(FrameRealm {
            node_id,
            context: ctx_global,
            fetch_rx,
            ws_evt_rx,
            alive: true,
        })
    });
    rv.set(v8::Boolean::new(scope, true).into());
}

/// `__framePostToFrame(nodeId, value)`. Page → frame: deliver a message into the frame context.
fn prim_frame_post_to_frame(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let node_id = args.get(0).number_value(scope).unwrap_or(0.0) as u32;
    let value = args.get(1);
    let ctx_g = FRAME_REG.with(|r| {
        r.borrow()
            .iter()
            .find(|f| f.node_id == node_id && f.alive)
            .map(|f| f.context.clone())
    });
    if let Some(ctx_g) = ctx_g {
        let ctx = v8::Local::new(scope, &ctx_g);
        let cscope = &mut v8::ContextScope::new(scope, ctx);
        crate::worker::call_global_fn(cscope, "__frameAccept", &[value]);
    }
}

/// `__framePostToParent(nodeId, value)`. Frame → page: deliver a message to the page window, with
/// the source frame's node id so the page can set `event.source` to its contentWindow.
fn prim_frame_post_to_parent(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let node_id = args.get(0).number_value(scope).unwrap_or(0.0);
    let value = args.get(1);
    let page_g = crate::worker::page_context();
    if let Some(page_g) = page_g {
        let ctx = v8::Local::new(scope, &page_g);
        let cscope = &mut v8::ContextScope::new(scope, ctx);
        let id_arg = v8::Number::new(cscope, node_id).into();
        crate::worker::call_global_fn(cscope, "__frameDeliverToParent", &[id_arg, value]);
    }
}

/// `__frameUnload(nodeId)`. Drop a frame realm (the `<iframe>` was removed or re-navigated).
fn prim_frame_unload(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let node_id = args.get(0).number_value(scope).unwrap_or(0.0) as u32;
    FRAME_REG.with(|r| r.borrow_mut().retain(|f| f.node_id != node_id));
}

/// Advance every live frame's loop one pass (fetches + ws + timers + microtasks), each in its own
/// context. Returns `(did_work, in_flight)` like [`crate::worker::pump_workers`].
pub fn pump_frames(scope: &mut v8::PinScope) -> (bool, usize) {
    let mut ctxs: Vec<v8::Global<v8::Context>> = Vec::new();
    let mut fetches: Vec<(v8::Global<v8::Context>, FetchCompletion)> = Vec::new();
    let mut ws_events: Vec<(v8::Global<v8::Context>, WsEvent)> = Vec::new();
    FRAME_REG.with(|r| {
        for f in r.borrow().iter().filter(|f| f.alive) {
            ctxs.push(f.context.clone());
            while let Ok(c) = f.fetch_rx.try_recv() {
                fetches.push((f.context.clone(), c));
            }
            while let Ok(e) = f.ws_evt_rx.try_recv() {
                ws_events.push((f.context.clone(), e));
            }
        }
    });

    let mut did_work = false;
    for (ctx_g, completion) in fetches {
        let ctx = v8::Local::new(scope, &ctx_g);
        let cscope = &mut v8::ContextScope::new(scope, ctx);
        deliver_fetch_completion(cscope, completion);
        cscope.perform_microtask_checkpoint();
        did_work = true;
    }
    for (ctx_g, (id, kind, payload)) in ws_events {
        let ctx = v8::Local::new(scope, &ctx_g);
        let cscope = &mut v8::ContextScope::new(scope, ctx);
        deliver_ws_event(cscope, id, kind, &payload);
        cscope.perform_microtask_checkpoint();
        did_work = true;
    }
    let mut in_flight = 0usize;
    for ctx_g in ctxs {
        let ctx = v8::Local::new(scope, &ctx_g);
        let cscope = &mut v8::ContextScope::new(scope, ctx);
        if run_due_timers(cscope) {
            did_work = true;
        }
        cscope.perform_microtask_checkpoint();
        if let Some(state) = cscope.get_current_context().get_slot::<HostState>() {
            in_flight += state.in_flight.get();
        }
    }
    (did_work, in_flight)
}

/// Pull the `body` field out of a request envelope JSON (`{"ok":..,"body":"..."}`) without a JSON
/// dependency: find `"body"`, then decode the following JSON string literal.
fn extract_envelope_body(env: &str) -> Option<String> {
    let key = "\"body\"";
    let mut i = env.find(key)? + key.len();
    let bytes = env.as_bytes();
    while i < bytes.len() && bytes[i] != b'"' {
        // skip whitespace + the colon
        if bytes[i] == b':' || bytes[i].is_ascii_whitespace() {
            i += 1;
        } else {
            return None; // unexpected (body wasn't a string)
        }
    }
    if i >= bytes.len() {
        return None;
    }
    i += 1; // opening quote
    let mut out = String::new();
    let chars: Vec<char> = env[i..].chars().collect();
    let mut j = 0;
    while j < chars.len() {
        let c = chars[j];
        if c == '"' {
            return Some(out);
        }
        if c == '\\' {
            j += 1;
            if j >= chars.len() {
                break;
            }
            match chars[j] {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                '/' => out.push('/'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'u' => {
                    let hex: String = chars[j + 1..(j + 5).min(chars.len())].iter().collect();
                    if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                        if let Some(ch) = char::from_u32(cp) {
                            out.push(ch);
                        }
                    }
                    j += 4;
                }
                other => out.push(other),
            }
        } else {
            out.push(c);
        }
        j += 1;
    }
    None
}
