//! Iframes and auxiliary windows run in separate Lumen realms.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Receiver;

use crate::primitives::{arg_str, js_str, set_fn};
use crate::{
    deliver_fetch_completion, deliver_ws_event, eval_internal, eval_source, host_state,
    install_browser_environment, register_host_state, run_due_timers, with_realm, Ctx,
    FetchCompletion, HostState, NativeArgs, NativeReturn, Realm, SharedDoc, Value, ValueExt,
    WsEvent,
};

/// JS overlay run in the frame context after the browser env is installed: wires `parent`/`top`/
/// `frameElement`/`postMessage`-to-parent and fires the frame's own load lifecycle.
const FRAME_ENV_BOOTSTRAP: &str = include_str!("bootstrap/frame_env.js");

/// One live iframe browsing context, keyed by the host `<iframe>` element's node id (page side).
struct FrameRealm {
    node_id: u32,
    context: Realm,
    parent: Realm,
    same_origin_parent: bool,
    fetch_rx: Receiver<FetchCompletion>,
    ws_evt_rx: Receiver<WsEvent>,
    alive: bool,
}

thread_local! {
    static FRAME_REG: RefCell<Vec<FrameRealm>> = const { RefCell::new(Vec::new()) };
}

/// Drop all frame realms (called on session teardown, before the interpreter).
pub fn clear_frames() {
    FRAME_REG.with(|r| r.borrow_mut().clear());
}

/// Retire every child of a discarded browsing context, including nested frames.
pub(crate) fn retire_children(scope: &mut Ctx, parent: &Value) {
    let parent_addr = scope.object_addr(parent);
    let retired = FRAME_REG.with(|r| {
        let mut frames = r.borrow_mut();
        let (retired, live): (Vec<_>, Vec<_>) = std::mem::take(&mut *frames)
            .into_iter()
            .partition(|f| scope.object_addr(&f.parent.global) == parent_addr);
        *frames = live;
        retired
    });
    for frame in retired {
        frame.context.retire(scope);
    }
}

fn remove_frame(scope: &mut Ctx, node_id: u32, parent_addr: Option<usize>) {
    let retired = FRAME_REG.with(|r| {
        let mut frames = r.borrow_mut();
        let (retired, live): (Vec<_>, Vec<_>) =
            std::mem::take(&mut *frames).into_iter().partition(|f| {
                f.node_id == node_id && scope.object_addr(&f.parent.global) == parent_addr
            });
        *frames = live;
        retired
    });
    for frame in retired {
        frame.context.retire(scope);
    }
}

/// Install the iframe bridge natives onto a context's global (page + frame contexts, so nested
/// iframes work).
pub(crate) fn register_iframe_natives(scope: &mut Ctx, global: &Value) {
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
    set_fn(scope, global, "__frameSet", prim_frame_set);
}

/// `__frameGet(nodeId, prop)` -> the value of `globalThis[prop]` in the frame's context (or
/// undefined). Used by the page's `contentWindow` proxy so `frame.contentWindow.performance` /
/// `.location` / `.document` reach the frame realm's real objects (same interpreter → the value is
/// valid in the page context).
fn prim_frame_get(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node_id = args.get(0).number_value(scope).unwrap_or(0.0) as u32;
    let prop = arg_str(scope, &args, 1);
    let caller = scope.global_this();
    let caller_addr = scope.object_addr(&caller);
    let ctx_g = FRAME_REG.with(|r| {
        r.borrow()
            .iter()
            .find(|f| {
                f.node_id == node_id
                    && f.alive
                    && f.same_origin_parent
                    && scope.object_addr(&f.parent.global) == caller_addr
            })
            .map(|f| f.context.clone())
    });
    if let Some(ctx_g) = ctx_g {
        if let Ok(value) = with_realm(scope, &ctx_g, move |ctx| {
            let global = ctx.global_this();
            ctx.member_get(&global, &prop).unwrap_or(Value::Undefined)
        }) {
            rv.set(value);
        }
    }
}

/// `__frameSet(nodeId, prop, value)` writes a same-origin WindowProxy property into the frame's
/// actual global object. Origin policy is enforced by the page-side proxy before calling this.
fn prim_frame_set(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let node_id = args.get(0).number_value(scope).unwrap_or(0.0) as u32;
    let prop = arg_str(scope, &args, 1);
    let value = args.get(2);
    let caller = scope.global_this();
    let caller_addr = scope.object_addr(&caller);
    let ctx_g = FRAME_REG.with(|r| {
        r.borrow()
            .iter()
            .find(|f| {
                f.node_id == node_id
                    && f.alive
                    && f.same_origin_parent
                    && scope.object_addr(&f.parent.global) == caller_addr
            })
            .map(|f| f.context.clone())
    });
    let written = ctx_g
        .map(|realm| {
            with_realm(scope, &realm, move |ctx| {
                let global = ctx.global_this();
                ctx.member_set(&global, &prop, value).is_ok()
            })
            .unwrap_or(false)
        })
        .unwrap_or(false);
    rv.set_bool(written);
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
fn prim_iframe_load(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
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
    rv.set_bool(ok);
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
fn prim_window_open(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
    let url = arg_str(scope, &args, 0);
    let node_id = next_aux_window_id();
    let ok = build_browsing_context(scope, node_id, &url, None, true, "navigate");
    rv.set_double(if ok { node_id as f64 } else { 0.0 });
}

/// Build a child browsing-context realm (an `<iframe>` when `is_aux` is false, or a `window.open`
/// auxiliary window when true): resolve the document HTML from `srcdoc`/`url`, create a fresh
/// Lumen realm, install the browser env + frame overlay, run the document's classic scripts, and
/// register the realm under `node_id`. Returns whether a document was loaded.
fn build_browsing_context(
    scope: &mut Ctx,
    node_id: u32,
    url: &str,
    srcdoc: Option<String>,
    is_aux: bool,
    nav_type: &str,
) -> bool {
    let page_state = host_state(scope);
    let Ok(parent) = Realm::current(scope) else {
        return false;
    };
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
    let page_url = page_state.page_url.borrow().clone();
    let same_origin_parent = !opaque_origin
        && (srcdoc.is_some() || frame_url == "about:blank" || same_origin(&page_url, &frame_url));
    let Ok(ctx_global) = Realm::new(scope) else {
        return false;
    };
    let shared: SharedDoc = Rc::new(RefCell::new(doc));
    let state = HostState::with_fetcher(
        shared,
        Rc::clone(&page_state.fetcher),
        request_fetcher.clone(),
        fetch_tx,
        ws_connector,
        ws_tx,
        page_state.cookie_getter.clone(),
        page_state.cookie_setter.clone(),
    );
    register_host_state(scope, &ctx_global.global, state);
    let parent_addr = scope.object_addr(&parent.global);
    let had_prev = FRAME_REG.with(|r| {
        r.borrow()
            .iter()
            .any(|f| f.node_id == node_id && scope.object_addr(&f.parent.global) == parent_addr)
    });
    let redirect_count = if frame_url != url && same_origin(url, &frame_url) {
        1.0
    } else {
        0.0
    };
    let nav_type = nav_type.to_string();
    let initialized = with_realm(scope, &ctx_global, move |ctx| {
        let global = ctx.global_this();
        for (key, value) in [
            ("__navType", js_str(ctx, &nav_type)),
            ("__hadPreviousDoc", Value::Bool(had_prev)),
            ("__redirectCount", Value::Num(redirect_count)),
            ("__opaqueOrigin", Value::Bool(opaque_origin)),
        ] {
            let _ = ctx.member_set(&global, key, value);
        }
        install_browser_environment(ctx, &frame_url);
        let _ = ctx.member_set(&global, "__frameNodeId", Value::Num(node_id as f64));
        let _ = ctx.member_set(&global, "__frameIsAuxWindow", Value::Bool(is_aux));
        if let Some(charset) = frame_charset {
            let charset = js_str(ctx, &charset);
            let _ = ctx.member_set(&global, "__documentCharset", charset);
        }
        if !eval_internal(ctx, FRAME_ENV_BOOTSTRAP, "<frame-env>") {
            return false;
        }
        for (external, source) in scripts {
            if external {
                if let Some(envelope) = request_fetcher("GET", &source, "", "{}") {
                    if let Some(body) = extract_envelope_body(&envelope) {
                        eval_source(ctx, &body, &source);
                    }
                }
            } else {
                eval_source(ctx, &source, &frame_url);
            }
        }
        eval_internal(
            ctx,
            "if (typeof __fireLifecycleEvents === 'function') { __fireLifecycleEvents(); } \
             if (typeof __enterRealtime === 'function') { __enterRealtime(); }",
            "<frame-lifecycle>",
        )
    })
    .unwrap_or(false);
    if !initialized {
        ctx_global.retire(scope);
        scope.collect_garbage_for_host();
        return false;
    }

    // Replace the old document and all of its descendant browsing contexts.
    remove_frame(scope, node_id, parent_addr);
    FRAME_REG.with(|r| {
        r.borrow_mut().push(FrameRealm {
            node_id,
            context: ctx_global,
            parent,
            same_origin_parent,
            fetch_rx,
            ws_evt_rx,
            alive: true,
        })
    });
    scope.collect_garbage_for_host();
    true
}

/// `__framePostToFrame(nodeId, value)`. Page → frame: deliver a message into the frame context.
fn prim_frame_post_to_frame(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let node_id = args.get(0).number_value(scope).unwrap_or(0.0) as u32;
    let value = args.get(1);
    let caller = scope.global_this();
    let caller_addr = scope.object_addr(&caller);
    let ctx_g = FRAME_REG.with(|r| {
        r.borrow()
            .iter()
            .find(|f| {
                f.node_id == node_id
                    && f.alive
                    && scope.object_addr(&f.parent.global) == caller_addr
            })
            .map(|f| f.context.clone())
    });
    if let Some(ctx_g) = ctx_g {
        let _ = with_realm(scope, &ctx_g, move |ctx| {
            crate::worker::call_global_fn(ctx, "__frameAccept", &[value]);
        });
    }
}

/// `__framePostToParent(nodeId, value)`. Frame → page: deliver a message to the page window, with
/// the source frame's node id so the page can set `event.source` to its contentWindow.
fn prim_frame_post_to_parent(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let node_id = args.get(0).number_value(scope).unwrap_or(0.0);
    let value = args.get(1);
    let caller_addr = scope.object_addr(&scope.global_this());
    let page_g = FRAME_REG
        .with(|r| {
            r.borrow()
                .iter()
                .find(|f| scope.object_addr(&f.context.global) == caller_addr)
                .map(|f| f.parent.clone())
        })
        .or_else(crate::worker::page_context);
    if let Some(page_g) = page_g {
        let _ = with_realm(scope, &page_g, move |ctx| {
            crate::worker::call_global_fn(
                ctx,
                "__frameDeliverToParent",
                &[Value::Num(node_id), value],
            );
        });
    }
}

/// `__frameUnload(nodeId)`. Drop a frame realm (the `<iframe>` was removed or re-navigated).
fn prim_frame_unload(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let node_id = args.get(0).number_value(scope).unwrap_or(0.0) as u32;
    let parent_addr = scope.object_addr(&scope.global_this());
    remove_frame(scope, node_id, parent_addr);
    scope.collect_garbage_for_host();
}

/// Advance every live frame's loop one pass (fetches + ws + timers + microtasks), each in its own
/// context. Returns `(did_work, in_flight)` like [`crate::worker::pump_workers`].
fn realm_is_active(scope: &Ctx, realm: &Realm) -> bool {
    let address = scope.object_addr(&realm.global);
    FRAME_REG.with(|r| {
        r.borrow()
            .iter()
            .any(|entry| scope.object_addr(&entry.context.global) == address)
    })
}

pub fn pump_frames(scope: &mut Ctx) -> (bool, usize) {
    let mut ctxs: Vec<Realm> = Vec::new();
    let mut fetches: Vec<(Realm, FetchCompletion)> = Vec::new();
    let mut ws_events: Vec<(Realm, WsEvent)> = Vec::new();
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
    for (realm, completion) in fetches {
        if !realm_is_active(scope, &realm) {
            continue;
        }
        let _ = with_realm(scope, &realm, move |ctx| {
            deliver_fetch_completion(ctx, completion);
            ctx.drain_microtasks_for_host();
        });
        did_work = true;
    }
    for (realm, (id, kind, payload)) in ws_events {
        if !realm_is_active(scope, &realm) {
            continue;
        }
        let _ = with_realm(scope, &realm, move |ctx| {
            deliver_ws_event(ctx, id, kind, &payload);
            ctx.drain_microtasks_for_host();
        });
        did_work = true;
    }
    let mut in_flight = 0;
    for realm in ctxs {
        if !realm_is_active(scope, &realm) {
            continue;
        }
        if let Ok((ran, pending)) = with_realm(scope, &realm, |ctx| {
            let ran = run_due_timers(ctx);
            ctx.drain_microtasks_for_host();
            (ran, host_state(ctx).in_flight.get())
        }) {
            did_work |= ran;
            in_flight += pending;
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
    match (wurl::Url::parse(a), wurl::Url::parse(b)) {
        (Ok(a), Ok(b)) => {
            let origin = a.origin();
            origin != "null" && origin == b.origin()
        }
        _ => false,
    }
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

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn frame_native_access_respects_opaque_origins() {
        let doc = html::parse("<!doctype html><body></body>");
        let (_, results) = run_with_dom(
            doc,
            vec![r#"
            var same = document.createElement('iframe');
            same.srcdoc = '<p>same origin</p>';
            document.body.appendChild(same);
            var opaque = document.createElement('iframe');
            opaque.setAttribute('sandbox', 'allow-scripts');
            opaque.srcdoc = '<p>opaque origin</p>';
            document.body.appendChild(opaque);
            setTimeout(function () {
                console.log(typeof __frameGet(same.__node, 'document'));
                console.log(typeof __frameGet(opaque.__node, 'document'));
                console.log(__frameSet(opaque.__node, 'secret', 'changed'));
                console.log(opaque.contentDocument === null);
            }, 0);
        "#
            .to_string()],
            "https://example.test/",
        );
        assert!(results.iter().all(|r| r.error.is_none()), "{results:?}");
        let console: Vec<_> = results.into_iter().flat_map(|r| r.console).collect();
        assert_eq!(console, ["object", "undefined", "false", "true"]);
    }
}

#[cfg(test)]
mod retirement_tests {
    use super::*;
    use crate::*;

    #[test]
    fn replacing_and_unloading_frames_releases_old_documents() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        install_host_state(
            ctx,
            HostState::new(Rc::new(RefCell::new(html::parse("<body></body>")))),
        );
        install_browser_environment(ctx, "https://example.test/");
        crate::set_page_context(Realm::current(ctx).unwrap_or_else(|_| panic!("page realm")));
        assert!(eval_source(
            ctx,
            "var f = document.createElement('iframe'); document.body.appendChild(f);",
            "fixture"
        )
        .error
        .is_none());
        for _ in 0..6 {
            assert!(eval_source(
                ctx,
                "__iframeLoad(f.__node, 'about:blank', '<body>first</body>');",
                "load"
            )
            .error
            .is_none());
            let old = FRAME_REG.with(|r| {
                let frame = r.borrow().last().unwrap().context.clone();
                let state = ctx
                    .realm_resource_for_host::<HostState>(&frame.global)
                    .unwrap();
                Rc::downgrade(&state)
            });
            assert!(eval_source(
                ctx,
                "__iframeLoad(f.__node, 'about:blank', '<body>second</body>');",
                "replace"
            )
            .error
            .is_none());
            ctx.collect_garbage_for_host();
            assert!(
                old.upgrade().is_none(),
                "renavigation must retire the old document"
            );
            let last = FRAME_REG.with(|r| {
                let frame = r.borrow().last().unwrap().context.clone();
                let state = ctx
                    .realm_resource_for_host::<HostState>(&frame.global)
                    .unwrap();
                Rc::downgrade(&state)
            });
            assert!(eval_source(ctx, "__frameUnload(f.__node);", "unload")
                .error
                .is_none());
            ctx.collect_garbage_for_host();
            assert!(
                last.upgrade().is_none(),
                "unloaded document must be reclaimed"
            );
            assert!(FRAME_REG.with(|r| r.borrow().is_empty()));
        }
        clear_frames();
        crate::clear_workers();
    }
}
