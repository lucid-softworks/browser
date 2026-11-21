//! Dedicated workers run in separate Lumen realms, pumped on the page session thread.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Receiver;

use crate::primitives::{arg_str, js_str, set_fn};
use crate::{
    deliver_fetch_completion, deliver_ws_event, eval_source, host_state,
    install_browser_environment, register_host_state, run_due_timers, with_realm, Ctx,
    FetchCompletion, HostState, NativeArgs, NativeReturn, Realm, SharedDoc, Value, ValueExt,
    WsEvent,
};

/// JS overlay turning a freshly browser-env'd context into a DedicatedWorkerGlobalScope, then
/// fetching + running the worker's top-level script. Runs in the worker context.
const WORKER_ENV_BOOTSTRAP: &str = include_str!("bootstrap/worker_env.js");

/// One live worker: its own realm. The interpreter registers a `HostState` per global and
/// keeps the worker global alive for the whole worker lifetime. `fetch_rx`/`ws_evt_rx` are the
/// worker's own async-IO channels, drained by [`pump_workers`] in the worker's context so `fetch()`
/// / `XMLHttpRequest` / `WebSocket` resolve inside the worker just like on the page.
struct WorkerRealm {
    id: u32,
    context: Realm,
    parent: Realm,
    fetch_rx: Receiver<FetchCompletion>,
    ws_evt_rx: Receiver<WsEvent>,
    alive: bool,
}

#[derive(Default)]
struct WorkerReg {
    /// The page (top-level) context, so a worker's `postMessage` can re-enter it to deliver.
    page_context: Option<Realm>,
    realms: Vec<WorkerRealm>,
}

#[derive(Clone, Default)]
struct WorkerRegistry(Rc<RefCell<WorkerReg>>);

fn with_workers<T>(
    scope: &mut Ctx,
    callback: impl FnOnce(&RefCell<WorkerReg>, &mut Ctx) -> T,
) -> T {
    if !scope.op_state().has::<WorkerRegistry>() {
        scope.op_state().put(WorkerRegistry::default());
    }
    let registry = scope.op_state().get::<WorkerRegistry>().unwrap().0.clone();
    callback(&registry, scope)
}

/// Register the page (top-level) context so worker → page message delivery can find it. Called once
/// after the session context is created.
pub(crate) fn set_page_context(scope: &mut Ctx, ctx: Realm) {
    with_workers(scope, |r, _scope| r.borrow_mut().page_context = Some(ctx));
}

/// Drop all worker realms and the page-context reference when a session tears down.
pub fn clear_workers(scope: &mut Ctx) {
    with_workers(scope, |r, _scope| {
        let mut reg = r.borrow_mut();
        reg.realms.clear();
        reg.page_context = None;
    });
}

/// Stop workers owned by a discarded document/worker, recursively.
pub(crate) fn retire_children(scope: &mut Ctx, parent: &Value) {
    let parent_addr = scope.object_addr(parent);
    let retired = with_workers(scope, |r, _scope| {
        let mut reg = r.borrow_mut();
        let (retired, live): (Vec<_>, Vec<_>) = std::mem::take(&mut reg.realms)
            .into_iter()
            .partition(|w| _scope.object_addr(&w.parent.global) == parent_addr);
        reg.realms = live;
        retired
    });
    for worker in retired {
        worker.context.retire(scope);
    }
}

/// Install the worker bridge natives onto a context's global (page and worker contexts alike, so a
/// worker can spawn sub-workers).
pub(crate) fn register_worker_natives(scope: &mut Ctx, global: &Value) {
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
fn prim_worker_create(scope: &mut Ctx, args: NativeArgs, rv: &mut NativeReturn) {
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
    let Ok(parent) = Realm::current(scope) else {
        rv.set_bool(false);
        return;
    };
    let request_fetcher = std::sync::Arc::clone(&page_state.request_fetcher);
    let ws_connector = std::sync::Arc::clone(&page_state.ws_connector);
    let cookie_getter = std::sync::Arc::clone(&page_state.cookie_getter);
    let cookie_setter = std::sync::Arc::clone(&page_state.cookie_setter);
    let (fetch_tx, fetch_rx) = std::sync::mpsc::channel();
    let (ws_tx, ws_evt_rx) = std::sync::mpsc::channel();

    let Ok(ctx_global) = Realm::new(scope) else {
        rv.set_bool(false);
        return;
    };
    let doc: SharedDoc = Rc::new(RefCell::new(dom::Document::new()));
    let state = HostState::with_fetcher(
        doc,
        Rc::clone(&page_state.fetcher),
        request_fetcher,
        fetch_tx,
        ws_connector,
        ws_tx,
        cookie_getter,
        cookie_setter,
    );
    register_host_state(scope, &ctx_global.global, state);
    let initialized = with_realm(scope, &ctx_global, move |cscope| {
        install_browser_environment(cscope, &script_url);
        let g = cscope.global_this();
        let _ = cscope.member_set(&g, "__workerId", Value::Num(id as f64));
        let script_url_value = js_str(cscope, &script_url);
        let _ = cscope.member_set(&g, "__workerScriptURL", script_url_value);
        if let Some(src) = inline_src {
            let source_value = js_str(cscope, &src);
            let _ = cscope.member_set(&g, "__workerInlineSource", source_value);
        }
        crate::eval_loop::eval_internal(cscope, WORKER_ENV_BOOTSTRAP, "<worker-env>")
    })
    .unwrap_or(false);
    if !initialized {
        ctx_global.retire(scope);
        scope.collect_garbage_for_host();
        rv.set_bool(false);
        return;
    }

    with_workers(scope, |r, _scope| {
        r.borrow_mut().realms.push(WorkerRealm {
            id,
            context: ctx_global,
            parent,
            fetch_rx,
            ws_evt_rx,
            alive: true,
        })
    });
    rv.set_bool(true);
}

/// `__runWorkerScript(src, url)`. Run `src` as a TOP-LEVEL script in the current (worker) context so
/// its declarations become worker-global properties. Used for the top script and each importScripts.
fn prim_run_worker_script(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let src = arg_str(scope, &args, 0);
    let url = arg_str(scope, &args, 1);
    let out = eval_source(scope, &src, &url);
    // Surface an uncaught top-level error into the worker console so it isn't silently lost.
    if let Some(err) = out.error {
        {
            let state = host_state(scope);
            state
                .console
                .borrow_mut()
                .push(format!("⚠ worker script error: {err}"));
        }
    }
}

/// `__workerPostToWorker(id, value)`. Page → worker: enter the worker context and hand it the value
/// (the worker localises it with its own structuredClone). Called in the page context.
fn prim_worker_post_to_worker(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u32;
    let value = args.get(1);
    let ctx_g = with_workers(scope, |r, _scope| {
        r.borrow()
            .realms
            .iter()
            .find(|w| w.id == id && w.alive)
            .map(|w| w.context.clone())
    });
    if let Some(ctx_g) = ctx_g {
        let _ = with_realm(scope, &ctx_g, move |cscope| {
            call_global_fn(cscope, "__workerAccept", &[value]);
        });
    }
}

/// `__workerPostToParent(id, value)`. Worker → page: enter the page context and deliver. Called in a
/// worker context.
fn prim_worker_post_to_parent(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0);
    let value = args.get(1);
    let page_g = with_workers(scope, |r, _scope| r.borrow().page_context.clone());
    if let Some(page_g) = page_g {
        let _ = with_realm(scope, &page_g, move |cscope| {
            call_global_fn(cscope, "__workerDeliver", &[Value::Num(id), value]);
        });
    }
}

/// `__workerTerminate(id)`. Mark a worker dead and drop its context.
fn prim_worker_terminate(scope: &mut Ctx, args: NativeArgs, _rv: &mut NativeReturn) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u32;
    let parent_addr = scope.object_addr(&scope.global_this());
    let retired = with_workers(scope, |r, _scope| {
        let mut reg = r.borrow_mut();
        let (retired, live): (Vec<_>, Vec<_>) = std::mem::take(&mut reg.realms)
            .into_iter()
            .partition(|w| w.id == id && _scope.object_addr(&w.parent.global) == parent_addr);
        reg.realms = live;
        retired
    });
    for worker in retired {
        worker.context.retire(scope);
    }
    scope.collect_garbage_for_host();
}

/// The page (top-level) context, if registered. Shared with the iframe realm bridge.
pub(crate) fn page_context(scope: &mut Ctx) -> Option<Realm> {
    with_workers(scope, |r, _scope| r.borrow().page_context.clone())
}

/// Call a global function `name` in the current context with `args`, ignoring the result and any
/// thrown exception (a misbehaving handler must not unwind through the host).
pub(crate) fn call_global_fn(scope: &mut Ctx, name: &str, args: &[Value]) {
    let global = scope.global_this();
    if let Ok(func) = scope.member_get(&global, name) {
        if func.is_callable() {
            let _ = scope.invoke(func, global, args);
        }
    }
}

/// Advance every live worker's event loop one pass: deliver any completed fetches / WebSocket events,
/// run due timers, and a microtask checkpoint — each in the worker's own context. Returns
/// `(did_work, in_flight)`: `did_work` keeps the page drain looping while a worker still has pending
/// tasks (queued message deliveries, timers, settled fetches), and `in_flight` is the total
/// outstanding async fetches across all workers so the drain keeps waiting (on the longer network
/// budget) until they settle. Called from [`crate::drain_event_loop`].
fn realm_is_active(scope: &mut Ctx, realm: &Realm) -> bool {
    let address = scope.object_addr(&realm.global);
    with_workers(scope, |r, _scope| {
        r.borrow()
            .realms
            .iter()
            .any(|entry| _scope.object_addr(&entry.context.global) == address)
    })
}

pub fn pump_workers(scope: &mut Ctx) -> (bool, usize) {
    // Phase 1: collect completed fetches / ws events per realm under a short borrow (try_recv runs
    // no JS). Running worker JS may re-enter WORKER_REG (spawn a sub-worker, terminate), so we must
    // not hold the borrow across the delivery in phase 2.
    let mut ctxs: Vec<Realm> = Vec::new();
    let mut fetches: Vec<(Realm, FetchCompletion)> = Vec::new();
    let mut ws_events: Vec<(Realm, WsEvent)> = Vec::new();
    with_workers(scope, |r, _scope| {
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

#[cfg(test)]
mod retirement_tests {
    use super::*;
    use crate::*;

    #[test]
    fn terminating_workers_releases_their_host_state() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        install_host_state(
            ctx,
            HostState::new(Rc::new(RefCell::new(html::parse("<body></body>")))),
        );
        install_browser_environment(ctx, "https://example.test/");
        let page = Realm::current(ctx).unwrap_or_else(|_| panic!("page realm"));
        set_page_context(ctx, page);
        for _ in 0..6 {
            let output = eval_source(ctx, "__workerCreate(1, 'https://example.test/worker.js', 'var held = new Array(1000).fill(42);')", "create");
            assert_eq!(output.value.as_deref(), Some("true"), "{output:?}");
            let weak = with_workers(ctx, |r, ctx| {
                let worker = r.borrow().realms.last().unwrap().context.clone();
                let state = ctx
                    .realm_resource_for_host::<HostState>(&worker.global)
                    .unwrap();
                Rc::downgrade(&state)
            });
            assert!(eval_source(ctx, "__workerTerminate(1)", "terminate")
                .error
                .is_none());
            ctx.collect_garbage_for_host();
            assert!(
                weak.upgrade().is_none(),
                "terminated worker must release host data"
            );
        }
        clear_workers(ctx);
        crate::clear_frames(ctx);
    }
    #[test]
    fn discarding_frame_retires_workers_and_their_nested_workers() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        install_host_state(
            ctx,
            HostState::new(Rc::new(RefCell::new(html::parse("<body></body>")))),
        );
        install_browser_environment(ctx, "https://example.test/");
        let page = Realm::current(ctx).unwrap_or_else(|_| panic!("page realm"));
        set_page_context(ctx, page);
        let setup = eval_source(
            ctx,
            r#"
            var f = document.createElement('iframe');
            document.body.appendChild(f);
            __iframeLoad(f.__node, 'about:blank', '<body>frame</body>');
            __frameGet(f.__node, '__workerCreate')(1, 'https://example.test/outer.js',
                '__workerCreate(1, "https://example.test/inner.js", "var held = new Array(1000).fill(42)")');
        "#,
            "nested workers",
        );
        assert!(setup.error.is_none(), "{setup:?}");
        let states = with_workers(ctx, |r, ctx| {
            r.borrow()
                .realms
                .iter()
                .map(|worker| {
                    let state = ctx
                        .realm_resource_for_host::<HostState>(&worker.context.global)
                        .unwrap();
                    Rc::downgrade(&state)
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(states.len(), 2);
        assert!(eval_source(ctx, "__frameUnload(f.__node)", "discard owner")
            .error
            .is_none());
        ctx.collect_garbage_for_host();
        assert!(with_workers(ctx, |r, _ctx| r.borrow().realms.is_empty()));
        assert!(
            states.iter().all(|state| state.upgrade().is_none()),
            "nested workers release host data with their owner"
        );
        clear_workers(ctx);
        crate::clear_frames(ctx);
    }
}
