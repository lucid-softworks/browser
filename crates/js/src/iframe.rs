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

use crate::primitives::{arg_str, set_fn};
use crate::{
    deliver_fetch_completion, deliver_ws_event, eval_internal, eval_source, host_state,
    install_browser_environment, run_due_timers, FetchCompletion, HostState, SharedDoc, WsEvent,
};

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
    set_fn(scope, global, "__windowOpen", prim_window_open);
    set_fn(
        scope,
        global,
        "__framePostToFrame",
        prim_frame_post_to_frame,
    );
    set_fn(
        scope,
        global,
        "__framePostToParent",
        prim_frame_post_to_parent,
    );
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
                if ty.map(|t| t.trim().eq_ignore_ascii_case("module")) != Some(true)
                    && is_js_type(ty)
                {
                    if let Some(src) = e.attrs.get("src") {
                        if let Some(abs) = wurl::resolve(src, base) {
                            out.push((true, abs));
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
    let nav_type = {
        let a = args.get(3);
        if a.is_string() {
            a.to_rust_string_lossy(scope)
        } else {
            "navigate".to_string()
        }
    };
    let ok = build_browsing_context(scope, node_id, &url, srcdoc, false, &nav_type);
    rv.set(v8::Boolean::new(scope, ok).into());
}

/// Synthetic node id for an auxiliary window (`window.open`). Uses a high base so it never collides
/// with a real `<iframe>` element's DOM node id (those are small and assigned per document).
fn next_aux_window_id() -> u32 {
    thread_local! {
        static COUNTER: std::cell::Cell<u32> = const { std::cell::Cell::new(0x7000_0000) };
    }
    COUNTER.with(|c| {
        let v = c.get();
        c.set(v.wrapping_add(1));
        v
    })
}

/// `__windowOpen(url) -> number`. Open an auxiliary top-level browsing context (a `window.open`
/// target): allocate a synthetic id, load+run its document like a frame, and register the realm.
/// Returns the synthetic id (used by the page's window proxy to message/read the new window), or 0
/// if the document failed to load.
fn prim_window_open(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let url = arg_str(scope, &args, 0);
    let node_id = next_aux_window_id();
    let ok = build_browsing_context(scope, node_id, &url, None, true, "navigate");
    rv.set(v8::Number::new(scope, if ok { node_id as f64 } else { 0.0 }).into());
}

/// Build a child browsing-context realm (an `<iframe>` when `is_aux` is false, or a `window.open`
/// auxiliary window when true): resolve the document HTML from `srcdoc`/`url`, create a fresh
/// `v8::Context`, install the browser env + frame overlay, run the document's classic scripts, and
/// register the realm under `node_id`. Returns whether a document was loaded.
fn build_browsing_context(
    scope: &mut v8::PinScope,
    node_id: u32,
    url: &str,
    srcdoc: Option<String>,
    is_aux: bool,
    nav_type: &str,
) -> bool {
    let page_state = host_state(scope);
    let request_fetcher = std::sync::Arc::clone(&page_state.request_fetcher);
    let ws_connector = std::sync::Arc::clone(&page_state.ws_connector);

    // A sandboxed <iframe> without `allow-same-origin` has an opaque origin; storage APIs
    // (e.g. cookieStore) must then throw SecurityError. Read the element's `sandbox` attribute from
    // the parent document. (Auxiliary windows have synthetic node ids and are never sandboxed here.)
    let opaque_origin = !is_aux && {
        let doc = page_state.doc.borrow();
        match &doc.get(dom::NodeId(node_id as usize)).data {
            dom::NodeData::Element(e) => e
                .attrs
                .get("sandbox")
                .map(|s| {
                    !s.split_whitespace()
                        .any(|t| t.eq_ignore_ascii_case("allow-same-origin"))
                })
                .unwrap_or(false),
            _ => false,
        }
    };

    // Resolve the frame's document HTML + its base URL (+ the response's charset, if any).
    let (html_src, frame_url, frame_charset) = match &srcdoc {
        Some(s) => (s.clone(), url.to_string(), None),
        None => {
            if let Some((html, charset)) = decode_data_url(url) {
                (html, url.to_string(), charset)
            } else if url.is_empty() || url == "about:blank" {
                (String::new(), "about:blank".to_string(), None)
            } else {
                // Fetch the document HTML via the host fetcher (envelope JSON: {ok,status,body,...}).
                // The envelope's `url` is the post-redirect final URL — use it as the frame's base.
                match request_fetcher("GET", url, "", "{}") {
                    Some(env) => match extract_envelope_body(&env) {
                        Some(body) => {
                            let final_u = extract_envelope_url(&env).filter(|u| !u.is_empty());
                            (
                                body,
                                final_u.unwrap_or_else(|| url.to_string()),
                                extract_envelope_charset(&env),
                            )
                        }
                        None => return false,
                    },
                    None => return false,
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
        let cookie_getter = std::sync::Arc::clone(&page_state.cookie_getter);
        let cookie_setter = std::sync::Arc::clone(&page_state.cookie_setter);
        let state = HostState::with_fetcher(
            Rc::clone(&shared),
            Rc::new(|_| None),
            request_fetcher.clone(),
            fetch_tx,
            ws_connector,
            ws_tx,
            cookie_getter,
            cookie_setter,
        );
        cscope.get_current_context().set_slot(state);
        // Seed the navigation type + whether a previous document existed for this frame (a same-origin
        // replacement unloads the old document → non-zero unloadEvent timings), before the env bootstrap
        // so Navigation Timing reflects them.
        let had_prev = FRAME_REG.with(|r| r.borrow().iter().any(|f| f.node_id == node_id));
        // A same-origin redirect (the final URL differs from the requested one, same origin) is
        // observable in Navigation Timing; a cross-origin redirect hides the count/timing (→ 0).
        let redirect_count = if frame_url != url && same_origin(url, &frame_url) {
            1
        } else {
            0
        };
        {
            let g0 = cscope.get_current_context().global(cscope);
            let knt = v8::String::new(cscope, "__navType").unwrap();
            let vnt = crate::js_str(cscope, nav_type);
            g0.set(cscope, knt.into(), vnt);
            let khp = v8::String::new(cscope, "__hadPreviousDoc").unwrap();
            let vhp = v8::Boolean::new(cscope, had_prev);
            g0.set(cscope, khp.into(), vhp.into());
            let krc = v8::String::new(cscope, "__redirectCount").unwrap();
            let vrc = v8::Number::new(cscope, redirect_count as f64);
            g0.set(cscope, krc.into(), vrc.into());
            if opaque_origin {
                if let Some(kop) = v8::String::new(cscope, "__opaqueOrigin") {
                    let vop = v8::Boolean::new(cscope, true);
                    g0.set(cscope, kop.into(), vop.into());
                }
            }
        }
        install_browser_environment(cscope, &frame_url);
        // Seed the host node id so the overlay can wire frameElement/parent messaging.
        let g = cscope.get_current_context().global(cscope);
        let kid = v8::String::new(cscope, "__frameNodeId").unwrap();
        let vid = v8::Number::new(cscope, node_id as f64);
        g.set(cscope, kid.into(), vid.into());
        // Auxiliary windows (window.open) get top-level semantics + an `opener`; the overlay keys
        // off this flag instead of wiring parent/top/frameElement to the page.
        if is_aux {
            if let Some(kaux) = v8::String::new(cscope, "__frameIsAuxWindow") {
                let vaux = v8::Boolean::new(cscope, true);
                g.set(cscope, kaux.into(), vaux.into());
            }
        }
        // Seed the document's charset so the frame's URL parsing encodes queries with it.
        if let Some(cs) = &frame_charset {
            if let Some(kcs) = v8::String::new(cscope, "__documentCharset") {
                let vcs = crate::js_str(cscope, cs);
                g.set(cscope, kcs.into(), vcs);
            }
        }
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
    true
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
/// Decode a `data:` URL into its document text + charset (None for a non-data URL). Handles the
/// `data:[<mediatype>][;base64],<data>` form; the fragment is the document's, not part of the body.
fn decode_data_url(url: &str) -> Option<(String, Option<String>)> {
    let rest = url
        .strip_prefix("data:")
        .or_else(|| url.strip_prefix("DATA:"))?;
    // The fragment is not part of the data.
    let rest = rest.split('#').next().unwrap_or(rest);
    let comma = rest.find(',')?;
    let meta = &rest[..comma];
    let data = &rest[comma + 1..];
    let meta_l = meta.to_ascii_lowercase();
    let charset = meta_l
        .find("charset=")
        .map(|i| {
            meta_l[i + "charset=".len()..]
                .split(';')
                .next()
                .unwrap_or("")
                .to_string()
        })
        .filter(|s| !s.is_empty());
    let bytes = if meta_l.trim_end().ends_with(";base64") {
        base64_decode(data)?
    } else {
        // application/x-www-form-urlencoded-style spaces aren't special here; just percent-decode.
        percent_decode_bytes(data)
    };
    Some((String::from_utf8_lossy(&bytes).into_owned(), charset))
}

fn percent_decode_bytes(input: &str) -> Vec<u8> {
    let b = input.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && (b[i + 1] as char).is_ascii_hexdigit()
            && (b[i + 2] as char).is_ascii_hexdigit()
        {
            let h = (b[i + 1] as char).to_digit(16).unwrap() as u8;
            let l = (b[i + 2] as char).to_digit(16).unwrap() as u8;
            out.push(h * 16 + l);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for &c in input.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = val(c)?;
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Pull `charset=<label>` out of the envelope's `contentType` field (e.g. "text/html;charset=big5").
fn extract_envelope_charset(env: &str) -> Option<String> {
    let key = "\"contentType\"";
    let after = &env[env.find(key)? + key.len()..];
    let start = after.find('"')? + 1;
    let value = &after[start..];
    let end = value.find('"')?;
    let ct = &value[..end];
    let cs = ct.to_ascii_lowercase();
    let idx = cs.find("charset=")? + "charset=".len();
    let label: String = cs[idx..]
        .chars()
        .take_while(|&c| c != ';' && c != ' ')
        .collect();
    if label.is_empty() {
        None
    } else {
        Some(label)
    }
}

fn extract_envelope_body(env: &str) -> Option<String> {
    extract_envelope_string(env, "\"body\"")
}

/// Whether two URLs share an origin (scheme + authority), for the redirect-timing same-origin check.
fn same_origin(a: &str, b: &str) -> bool {
    fn origin(u: &str) -> Option<(String, String)> {
        let se = u.find("://")?;
        let authority = u[se + 3..].split(['/', '?', '#']).next().unwrap_or("");
        Some((u[..se].to_ascii_lowercase(), authority.to_ascii_lowercase()))
    }
    matches!((origin(a), origin(b)), (Some(x), Some(y)) if x == y)
}

/// The post-redirect final URL recorded in the response envelope (`"url"` field).
fn extract_envelope_url(env: &str) -> Option<String> {
    extract_envelope_string(env, "\"url\"")
}

fn extract_envelope_string(env: &str, key: &str) -> Option<String> {
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
