//! Dedicated Web Workers as real, separate V8 *contexts* in the page's isolate.
//!
//! A dedicated worker needs its own global object so that `self === globalThis` holds the way it
//! does in a real worker: top-level `var`/`function` declarations in the worker script (and in every
//! `importScripts`'d file) must become properties of that global, visible across files. A single
//! shared context (the page) cannot provide that — a function-wrapped scope makes declarations
//! local. So each worker gets its own `v8::Context` in the SAME isolate, on the SAME session thread,
//! and the existing event-loop drain pumps it cooperatively (no OS thread, no parallelism — V8
//! isolates are thread-bound and the loop is already cooperative single-thread).
//!
//! - The worker context has the full browser environment installed plus a worker overlay
//!   (`worker_env.js`): `self`/`globalThis`/`location`, `postMessage`/`close`/`importScripts`, and
//!   `DedicatedWorkerGlobalScope` (recognised by testharness, which selects the worker test
//!   environment via `self instanceof DedicatedWorkerGlobalScope`).
//! - `importScripts` and the top-level worker script run as TOP-LEVEL scripts in the worker context
//!   (via [`prim_run_worker_script`]), so their declarations land on the worker global.
//! - Messages cross contexts by handing the raw value to the receiver context and letting the
//!   receiver's own `structuredClone` deep-copy it (same isolate → the handle is valid in both).
//!
//! The page context is registered via [`set_page_context`]; worker realms live in a thread-local
//! (everything is on the one session thread, and a `v8::Global` is not `Send`). [`pump_workers`] is
//! called from [`crate::drain_event_loop`] to advance each worker's timers/microtasks.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Receiver;

use crate::primitives::{arg_str, set_fn};
use crate::{
    deliver_fetch_completion, deliver_ws_event, eval_source, host_state,
    install_browser_environment, js_str, run_due_timers, FetchCompletion, HostState, SharedDoc,
    WsEvent,
};

/// JS overlay turning a freshly browser-env'd context into a DedicatedWorkerGlobalScope, then
/// fetching + running the worker's top-level script. Runs in the worker context.
const WORKER_ENV_BOOTSTRAP: &str = include_str!("bootstrap/worker_env.js");

/// One live worker: its own context. The context owns its `HostState` (set as a context slot) and
/// keeps the worker global alive for the whole worker lifetime. `fetch_rx`/`ws_evt_rx` are the
/// worker's own async-IO channels, drained by [`pump_workers`] in the worker's context so `fetch()`
/// / `XMLHttpRequest` / `WebSocket` resolve inside the worker just like on the page.
struct WorkerRealm {
    id: u32,
    context: v8::Global<v8::Context>,
    fetch_rx: Receiver<FetchCompletion>,
    ws_evt_rx: Receiver<WsEvent>,
    alive: bool,
}

struct WorkerReg {
    /// The page (top-level) context, so a worker's `postMessage` can re-enter it to deliver.
    page_context: Option<v8::Global<v8::Context>>,
    realms: Vec<WorkerRealm>,
}

thread_local! {
    static WORKER_REG: RefCell<WorkerReg> =
        const { RefCell::new(WorkerReg { page_context: None, realms: Vec::new() }) };
}

/// Register the page (top-level) context so worker → page message delivery can find it. Called once
/// after the session context is created.
pub fn set_page_context(ctx: v8::Global<v8::Context>) {
    WORKER_REG.with(|r| r.borrow_mut().page_context = Some(ctx));
}

/// Drop all worker realms and the page-context reference. Called when a session tears down so the
/// thread-local doesn't outlive the isolate.
pub fn clear_workers() {
    WORKER_REG.with(|r| {
        let mut reg = r.borrow_mut();
        reg.realms.clear();
        reg.page_context = None;
    });
}

/// Install the worker bridge natives onto a context's global (page and worker contexts alike, so a
/// worker can spawn sub-workers).
pub(crate) fn register_worker_natives(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    set_fn(scope, global, "__workerCreate", prim_worker_create);
    set_fn(
        scope,
        global,
        "__workerPostToWorker",
        prim_worker_post_to_worker,
    );
    set_fn(
        scope,
        global,
        "__workerPostToParent",
        prim_worker_post_to_parent,
    );
    set_fn(scope, global, "__workerTerminate", prim_worker_terminate);
    set_fn(scope, global, "__runWorkerScript", prim_run_worker_script);
}

/// `__workerCreate(id, scriptURL) -> bool`. Build a new worker context (full browser env + worker
/// overlay), store it as a realm, and run its top-level script (the overlay fetches + runs it).
fn prim_worker_create(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u32;
    let script_url = arg_str(scope, &args, 1);
    // Optional inline source (e.g. a decoded data:/blob: worker) to run instead of fetching.
    let inline_src: Option<String> = {
        let a = args.get(2);
        if a.is_string() {
            Some(a.to_rust_string_lossy(scope))
        } else {
            None
        }
    };

    // Inherit the page's network capabilities (synchronous `__request` powers importScripts; the
    // async fetch/WebSocket channels are the worker's own, drained by pump_workers in its context).
    let page_state = host_state(scope);
    let request_fetcher = std::sync::Arc::clone(&page_state.request_fetcher);
    let ws_connector = std::sync::Arc::clone(&page_state.ws_connector);
    let (fetch_tx, fetch_rx) = std::sync::mpsc::channel();
    let (ws_tx, ws_evt_rx) = std::sync::mpsc::channel();

    let ctx_global = {
        let ctx = v8::Context::new(scope, Default::default());
        let cscope = &mut v8::ContextScope::new(scope, ctx);
        let doc: SharedDoc = Rc::new(RefCell::new(dom::Document::new()));
        let state = HostState::with_fetcher(
            doc,
            Rc::new(|_| None),
            request_fetcher,
            fetch_tx,
            ws_connector,
            ws_tx,
        );
        cscope.get_current_context().set_slot(state);
        install_browser_environment(cscope, &script_url);
        // Seed the worker id + script URL the overlay reads, then run the overlay (which converts the
        // global into a DedicatedWorkerGlobalScope and fetches + runs the top-level worker script).
        let g = cscope.get_current_context().global(cscope);
        let kid = v8::String::new(cscope, "__workerId").unwrap();
        let vid = v8::Number::new(cscope, id as f64);
        g.set(cscope, kid.into(), vid.into());
        let kurl = v8::String::new(cscope, "__workerScriptURL").unwrap();
        let vurl = js_str(cscope, &script_url);
        g.set(cscope, kurl.into(), vurl);
        if let Some(src) = &inline_src {
            let ks = v8::String::new(cscope, "__workerInlineSource").unwrap();
            let vs = js_str(cscope, src);
            g.set(cscope, ks.into(), vs);
        }
        crate::eval_loop::eval_internal(cscope, WORKER_ENV_BOOTSTRAP, "<worker-env>");
        v8::Global::new(cscope, ctx)
    };

    WORKER_REG.with(|r| {
        r.borrow_mut().realms.push(WorkerRealm {
            id,
            context: ctx_global,
            fetch_rx,
            ws_evt_rx,
            alive: true,
        })
    });
    rv.set(v8::Boolean::new(scope, true).into());
}

/// `__runWorkerScript(src, url)`. Run `src` as a TOP-LEVEL script in the current (worker) context so
/// its declarations become worker-global properties. Used for the top script and each importScripts.
fn prim_run_worker_script(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let src = arg_str(scope, &args, 0);
    let url = arg_str(scope, &args, 1);
    let out = eval_source(scope, &src, &url);
    // Surface an uncaught top-level error into the worker console so it isn't silently lost.
    if let Some(err) = out.error {
        if let Some(state) = scope.get_current_context().get_slot::<HostState>() {
            state
                .console
                .borrow_mut()
                .push(format!("⚠ worker script error: {err}"));
        }
    }
}

/// `__workerPostToWorker(id, value)`. Page → worker: enter the worker context and hand it the value
/// (the worker localises it with its own structuredClone). Called in the page context.
fn prim_worker_post_to_worker(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u32;
    let value = args.get(1);
    let ctx_g = WORKER_REG.with(|r| {
        r.borrow()
            .realms
            .iter()
            .find(|w| w.id == id && w.alive)
            .map(|w| w.context.clone())
    });
    if let Some(ctx_g) = ctx_g {
        let ctx = v8::Local::new(scope, &ctx_g);
        let cscope = &mut v8::ContextScope::new(scope, ctx);
        call_global_fn(cscope, "__workerAccept", &[value]);
    }
}

/// `__workerPostToParent(id, value)`. Worker → page: enter the page context and deliver. Called in a
/// worker context.
fn prim_worker_post_to_parent(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0);
    let value = args.get(1);
    let page_g = WORKER_REG.with(|r| r.borrow().page_context.clone());
    if let Some(page_g) = page_g {
        let ctx = v8::Local::new(scope, &page_g);
        let cscope = &mut v8::ContextScope::new(scope, ctx);
        let id_arg = v8::Number::new(cscope, id).into();
        call_global_fn(cscope, "__workerDeliver", &[id_arg, value]);
    }
}

/// `__workerTerminate(id)`. Mark a worker dead and drop its context.
fn prim_worker_terminate(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u32;
    WORKER_REG.with(|r| r.borrow_mut().realms.retain(|w| w.id != id));
}

/// The page (top-level) context, if registered. Shared with the iframe realm bridge.
pub(crate) fn page_context() -> Option<v8::Global<v8::Context>> {
    WORKER_REG.with(|r| r.borrow().page_context.clone())
}

/// Call a global function `name` in the current context with `args`, ignoring the result and any
/// thrown exception (a misbehaving handler must not unwind through the host).
pub(crate) fn call_global_fn(scope: &mut v8::PinScope, name: &str, args: &[v8::Local<v8::Value>]) {
    let global = scope.get_current_context().global(scope);
    let key = match v8::String::new(scope, name) {
        Some(k) => k,
        None => return,
    };
    let func = global
        .get(scope, key.into())
        .and_then(|v| v8::Local::<v8::Function>::try_from(v).ok());
    if let Some(func) = func {
        let recv = global.into();
        v8::tc_scope!(let tc, scope);
        func.call(tc, recv, args);
    }
}

/// Advance every live worker's event loop one pass: deliver any completed fetches / WebSocket events,
/// run due timers, and a microtask checkpoint — each in the worker's own context. Returns
/// `(did_work, in_flight)`: `did_work` keeps the page drain looping while a worker still has pending
/// tasks (queued message deliveries, timers, settled fetches), and `in_flight` is the total
/// outstanding async fetches across all workers so the drain keeps waiting (on the longer network
/// budget) until they settle. Called from [`crate::drain_event_loop`].
pub fn pump_workers(scope: &mut v8::PinScope) -> (bool, usize) {
    // Phase 1: collect completed fetches / ws events per realm under a short borrow (try_recv runs
    // no JS). Running worker JS may re-enter WORKER_REG (spawn a sub-worker, terminate), so we must
    // not hold the borrow across the delivery in phase 2.
    let mut ctxs: Vec<v8::Global<v8::Context>> = Vec::new();
    let mut fetches: Vec<(v8::Global<v8::Context>, FetchCompletion)> = Vec::new();
    let mut ws_events: Vec<(v8::Global<v8::Context>, WsEvent)> = Vec::new();
    WORKER_REG.with(|r| {
        for w in r.borrow().realms.iter().filter(|w| w.alive) {
            ctxs.push(w.context.clone());
            while let Ok(c) = w.fetch_rx.try_recv() {
                fetches.push((w.context.clone(), c));
            }
            while let Ok(e) = w.ws_evt_rx.try_recv() {
                ws_events.push((w.context.clone(), e));
            }
        }
    });

    let mut did_work = false;
    // Phase 2a: deliver fetch completions in each owning worker context.
    for (ctx_g, completion) in fetches {
        let ctx = v8::Local::new(scope, &ctx_g);
        let cscope = &mut v8::ContextScope::new(scope, ctx);
        deliver_fetch_completion(cscope, completion);
        cscope.perform_microtask_checkpoint();
        did_work = true;
    }
    // Phase 2b: deliver WebSocket events.
    for (ctx_g, (id, kind, payload)) in ws_events {
        let ctx = v8::Local::new(scope, &ctx_g);
        let cscope = &mut v8::ContextScope::new(scope, ctx);
        deliver_ws_event(cscope, id, kind, &payload);
        cscope.perform_microtask_checkpoint();
        did_work = true;
    }
    // Phase 2c: run timers + microtasks per worker, and tally outstanding fetches.
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
