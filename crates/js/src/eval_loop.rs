use crate::*;

/// Compile+run a script in the current context, ignoring the result. Used for bootstraps where a
/// failure would be a build-time bug (we surface it via a panic in debug-style assertions).
pub(crate) fn eval_internal(scope: &mut Ctx, source: &str, name: &str) -> bool {
    match eval_value(scope, source) {
        Ok(_) => true,
        Err(error) => {
            let text = error.to_rust_string_lossy(scope);
            host_state(scope)
                .console
                .borrow_mut()
                .push(format!("{name}: {text}"));
            false
        }
    }
}

pub(crate) fn install_browser_environment(scope: &mut Ctx, url: &str) {
    let global = scope.global_this();
    install_console_sink(scope, &global);
    install_dom_primitives(scope, &global);
    // Test harness host operations are retained privately, never exposed to page scripts.
    eval_internal(scope, "delete globalThis.$262", "<hide-host>");
    // Build `window`/`self`/`globalThis` aliases + the JS `document` over the primitives.
    eval_internal(scope, DOCUMENT_BOOTSTRAP, "<document>");
    // Timers / event loop.
    eval_internal(scope, TIMERS_BOOTSTRAP, "<timers>");
    let page_url = js_str(scope, url);
    scope
        .member_set(&global, "__pageURL", page_url)
        .ok()
        .expect("set URL");
    *host_state(scope).page_url.borrow_mut() = url.to_string();
    let (vw, vh, dpr) = device_metrics();
    for (name, num) in [
        ("__innerWidth", vw),
        ("__innerHeight", vh),
        ("__devicePixelRatio", dpr),
        ("__responseBodySize", response_body_size() as f64),
    ] {
        scope
            .member_set(&global, name, Value::Num(num))
            .ok()
            .expect("set metrics");
    }
    scope
        .member_set(
            &global,
            "__crossOriginIsolated",
            Value::Bool(cross_origin_isolated()),
        )
        .ok()
        .expect("set isolation");
    eval_internal(scope, BROWSER_ENV_BOOTSTRAP, "<browser-env>");
    // IndexedDB (in-memory) — depends on structuredClone + queueMicrotask from the prior bootstraps.
    eval_internal(scope, INDEXEDDB_BOOTSTRAP, "<indexeddb>");
    // Web Crypto SubtleCrypto (digest + HMAC) — depends on `crypto` from browser-env.
    eval_internal(scope, WEBCRYPTO_BOOTSTRAP, "<webcrypto>");
    // SVG DOM (SVG* interfaces, animated-attribute reflection, SMIL) — depends on browser-env.
    eval_internal(scope, SVG_BOOTSTRAP, "<svg>");
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
pub(crate) const DOCUMENT_BOOTSTRAP: &str = include_str!("bootstrap/document.js");

/// JS bootstrap implementing the timer / event-loop APIs. Engine-agnostic — reused verbatim.
/// All scheduling lives here; Rust only drives via `__runDueTimers()` and reads `__timerErrors`.
pub(crate) const TIMERS_BOOTSTRAP: &str = include_str!("bootstrap/timers.js");

/// JS bootstrap implementing the standard browser environment (navigator/location/history/
/// storage/screen/matchMedia/getComputedStyle/event model/observers/URL/etc.) plus the per-node
/// wrapper cache, `style`/`classList`/`dataset` write-through, and the DOM interface class
/// hierarchy. Engine-agnostic — reused verbatim from the prior implementation; it talks to the
/// document via the JS `document` layer + the node-id `document.__getAttr/__setAttr/__removeAttr`
/// helpers (now built over the native primitives in DOCUMENT_BOOTSTRAP).
pub(crate) const BROWSER_ENV_BOOTSTRAP: &str = include_str!("bootstrap/browser_env.js");

/// In-memory IndexedDB (`indexedDB`, IDBDatabase/ObjectStore/Index/Cursor/KeyRange/…). A pure-JS
/// implementation backed by per-realm in-memory stores; values are deep-copied with `structuredClone`
/// and requests/transactions run asynchronously on the microtask queue. Not persisted to disk.
pub(crate) const INDEXEDDB_BOOTSTRAP: &str = include_str!("bootstrap/indexeddb.js");

/// Web Crypto `crypto.subtle` (digest for SHA-1/256/384/512 and HMAC sign/verify/generate/import/
/// export), a pure-JS implementation layered onto the `crypto` object from browser-env (whose
/// getRandomValues already uses the OS CSPRNG via the `__cryptoRandom` native).
pub(crate) const WEBCRYPTO_BOOTSTRAP: &str = include_str!("bootstrap/webcrypto.js");

/// SVG DOM: the SVG* IDL interface constructors, animated-attribute reflection
/// (SVGAnimatedLength.baseVal/animVal), and a SMIL animation engine. Depends on the element
/// wrapper machinery + DOMException from <browser-env>; enrichElement calls into `__svgEnrich`.
pub(crate) const SVG_BOOTSTRAP: &str = include_str!("bootstrap/svg.js");

// ---------------------------------------------------------------------------------------------
// Event loop drain + script evaluation against a Lumen realm.
// ---------------------------------------------------------------------------------------------

/// Maximum number of `__runDueTimers()` iterations when draining the event loop.
pub(crate) const EVENT_LOOP_CAP: usize = 10_000;

/// Evaluate in the persistent realm, capturing thrown values and console output.
pub(crate) fn eval_source(scope: &mut Ctx, source: &str, _name: &str) -> EvalOutput {
    host_state(scope).console.borrow_mut().clear();
    let result = eval_value(scope, source);
    scope.drain_microtasks_for_host();
    let (value, error) = if scope.execution_cancelled() {
        (None, Some(EXECUTION_STOPPED.to_string()))
    } else {
        match result {
            Ok(v) => (
                if v.is_undefined() {
                    None
                } else {
                    Some(v.to_rust_string_lossy(scope))
                },
                None,
            ),
            Err(e) => (None, Some(e.to_rust_string_lossy(scope))),
        }
    };
    let console = std::mem::take(&mut *host_state(scope).console.borrow_mut());
    EvalOutput {
        value,
        error,
        console,
    }
}

/// Pump every dedicated-worker AND iframe realm one pass, returning `(did_work, total_in_flight)`.
/// Each realm advances its own timers / fetch completions / message deliveries in its own context.
pub(crate) fn pump_realms(scope: &mut Ctx) -> (bool, usize) {
    let (w_did, w_if) = crate::pump_workers(scope);
    let (f_did, f_if) = crate::pump_frames(scope);
    (w_did || f_did, w_if + f_if)
}

/// Drive the event loop to completion (or the time/iteration cap) after page sources have run.
/// Fires the DOM lifecycle events, then alternates Lumen microtask checkpoints with the JS
/// `__runDueTimers()` driver. Folds any console output + `__timerErrors` produced during the
/// drain into the last result (matching the prior behavior).
/// Returns whether any timer/microtask actually fired (so `tick` can skip a DOM snapshot when
/// nothing happened).
pub(crate) fn drain_event_loop(
    scope: &mut Ctx,
    results: &mut [EvalOutput],
    fetch_rx: Option<&Receiver<FetchCompletion>>,
    ws_evt_rx: Option<&Receiver<WsEvent>>,
) -> bool {
    if let Some(state) = host_state_opt(scope) {
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

    // Interpreter safe points enforce the host deadline even inside a single callback.
    // These shorter budgets additionally bound cooperative event-loop work.
    let start = std::time::Instant::now();
    // Idle budget keeps ticks snappy; the network budget is raised because a page legitimately
    // waiting on a slow request (the slowest imlunahey _serverFn is ~6.8s) needs longer than 3s.
    let idle_budget = std::time::Duration::from_millis(3000);
    let network_budget = std::time::Duration::from_millis(15000);
    let mut iterations = 0usize;
    let mut did_work = false;
    // Outstanding async fetches across all dedicated workers (updated each pass by pump_workers); the
    // loop stays alive on the network budget while either the page or a worker has requests pending.
    let mut worker_in_flight = 0usize;
    loop {
        if scope.execution_cancelled() {
            if let Some(last) = results.last_mut() {
                last.error = Some(EXECUTION_STOPPED.to_string());
            }
            break;
        }
        // Pull any completed background fetches and settle their JS promises, then run a microtask
        // checkpoint so the `.then` chains progress within this same drain.
        if resolve_completed_fetches(scope, fetch_rx) {
            did_work = true;
            scope.drain_microtasks_for_host();
        }

        // Opportunistically deliver any pending WebSocket events (non-blocking). A socket is
        // long-lived, so this never gates the loop (no `in_flight`); events simply arrive within
        // ~one drain pass. A delivered handler may queue microtasks, so checkpoint after.
        if deliver_ws_events(scope, ws_evt_rx) {
            did_work = true;
            scope.drain_microtasks_for_host();
        }

        let page_in_flight = host_state_opt(scope)
            .map(|s| s.in_flight.get())
            .unwrap_or(0);
        // While requests are outstanding (on the page OR in any worker) we use the longer budget (so
        // a network-bound page/worker isn't cut off); otherwise the short idle budget keeps idle
        // ticks cheap. Worker fetches resolve inside pump_workers below, which also reports the live
        // worker in-flight count.
        let in_flight = page_in_flight + worker_in_flight;
        let budget = if in_flight > 0 {
            network_budget
        } else {
            idle_budget
        };
        if iterations >= EVENT_LOOP_CAP || start.elapsed() >= budget {
            break;
        }
        // Run any pending Lumen microtasks/promise jobs first.
        scope.drain_microtasks_for_host();

        // Then run one due timer/microtask from the JS event loop, and advance any dedicated
        // workers' loops (their queued message deliveries / fetches / timers run in their own
        // contexts). pump_workers reports whether it did work and the worker in-flight tally.
        let ran = run_due_timers(scope);
        let (wran, wif) = pump_realms(scope);
        worker_in_flight = wif;
        iterations += 1;
        if ran || wran {
            did_work = true;
        } else {
            // Nothing left in the page loop; one more microtask checkpoint in case the last timer
            // queued a job, and one more worker pump in case a microtask queued worker work.
            scope.drain_microtasks_for_host();
            let (wran2, wif2) = pump_realms(scope);
            worker_in_flight = wif2;
            if run_due_timers(scope) || wran2 {
                did_work = true;
            } else if page_in_flight > 0 {
                // No JS work is due but a request is still in flight: block briefly on the channel
                // (instead of busy-spinning) for the next completion, then loop to resolve it.
                if let Some(rx) = fetch_rx {
                    match rx.recv_timeout(std::time::Duration::from_millis(20)) {
                        Ok(completion) => {
                            deliver_fetch_completion(scope, completion);
                            did_work = true;
                            scope.drain_microtasks_for_host();
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        // All senders gone (shouldn't happen mid-flight): stop waiting.
                        Err(_) => break,
                    }
                }
            } else if wif2 > 0 {
                // No page work, but a worker has a request in flight (its completion arrives on the
                // worker's own channel, drained by pump_workers). Sleep briefly to avoid busy-spin,
                // then loop so the next pump picks it up.
                std::thread::sleep(std::time::Duration::from_millis(5));
            } else {
                break;
            }
        }
    }

    // One final sweep: settle any completions that landed after the loop's last check, then run a
    // microtask checkpoint so their `.then` chains progress before we snapshot.
    if resolve_completed_fetches(scope, fetch_rx) {
        did_work = true;
        scope.drain_microtasks_for_host();
    }
    // Same final sweep for WebSocket events that arrived after the loop's last pass.
    if deliver_ws_events(scope, ws_evt_rx) {
        did_work = true;
        scope.drain_microtasks_for_host();
    }

    // MutationObserver delivery: callbacks fire as a microtask after the task. If the task queued
    // any DOM mutations, deliver them; a delivered callback may itself mutate the DOM, so loop
    // (bounded) until the queue drains or we hit the cap (guards against infinite mutation loops).
    {
        let has_mutations = |scope: &mut Ctx| {
            host_state_opt(scope)
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
            scope.drain_microtasks_for_host();
            // A callback may have scheduled timers; let one round of due work run so e.g. a
            // setTimeout(0) inside an observer still progresses within this drain.
            run_due_timers(scope);
            scope.drain_microtasks_for_host();
            did_work = true;
            rounds += 1;
        }
    }

    // Final worker drain: a last few passes so any worker task queued right at the end (e.g. a
    // testharness completion message posted to the page) is delivered, ping-ponging with the page
    // loop until both go quiet (bounded).
    {
        let mut rounds = 0usize;
        loop {
            let (w, wif) = pump_realms(scope);
            scope.drain_microtasks_for_host();
            let p = run_due_timers(scope);
            scope.drain_microtasks_for_host();
            if w || p {
                did_work = true;
            }
            rounds += 1;
            // Keep ping-ponging while there is work; if only worker fetches remain in flight, sleep
            // briefly so their completions can arrive before the next pass. Bounded by `rounds`.
            if !w && !p {
                if wif > 0 && rounds < 256 {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                } else {
                    break;
                }
            }
            if rounds >= 256 {
                break;
            }
        }
    }

    if scope.execution_cancelled() {
        if let Some(last) = results.last_mut() {
            last.error = Some(EXECUTION_STOPPED.to_string());
            last.console.push(EXECUTION_STOPPED.to_string());
        }
        return true;
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

    let drained = host_state_opt(scope)
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
pub(crate) fn run_due_timers(scope: &mut Ctx) -> bool {
    eval_to_bool(
        scope,
        "(typeof __runDueTimers === 'function') && __runDueTimers()",
    )
}

/// Drain all currently-available background fetch completions (non-blocking `try_recv`) and settle
/// each one's JS promise. Returns whether any completion was delivered. No-op when `fetch_rx` is
/// `None` (the no-DOM / eval paths that never start async fetches).
pub(crate) fn resolve_completed_fetches(
    scope: &mut Ctx,
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
pub(crate) fn deliver_fetch_completion(scope: &mut Ctx, completion: FetchCompletion) {
    let (id, env) = completion;
    if let Some(state) = host_state_opt(scope) {
        state.in_flight.set(state.in_flight.get().saturating_sub(1));
    }
    match env {
        Some(envelope) => {
            let envelope = js_str(scope, &envelope);
            call_global_fn(scope, "__resolveFetch", &[Value::Num(id as f64), envelope]);
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
pub(crate) fn deliver_ws_events(scope: &mut Ctx, ws_evt_rx: Option<&Receiver<WsEvent>>) -> bool {
    let rx = match ws_evt_rx {
        Some(rx) => rx,
        None => return false,
    };
    let mut any = false;
    while let Ok((id, kind, payload)) = rx.try_recv() {
        // On close, drop the outgoing sender for this id (the socket thread is finishing).
        if kind == 3 {
            if let Some(state) = host_state_opt(scope) {
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
pub(crate) fn deliver_ws_event(scope: &mut Ctx, id: u64, kind: u8, payload: &str) {
    let payload = js_str(scope, payload);
    call_global_fn(
        scope,
        "__wsDeliver",
        &[Value::Num(id as f64), Value::Num(kind as f64), payload],
    );
}

/// Evaluate an internal expression, returning its boolean coercion. Errors → false.
pub(crate) fn eval_to_bool(scope: &mut Ctx, source: &str) -> bool {
    eval_value(scope, source)
        .map(|v| v.boolean_value(scope))
        .unwrap_or(false)
}

pub(crate) fn eval_to_string(scope: &mut Ctx, source: &str) -> Option<String> {
    eval_value(scope, source)
        .ok()
        .map(|v| v.to_rust_string_lossy(scope))
}
