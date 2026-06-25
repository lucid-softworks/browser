use crate::*;

/// Compile+run a script in the current context, ignoring the result. Used for bootstraps where a
/// failure would be a build-time bug (we surface it via a panic in debug-style assertions).
pub(crate) fn eval_internal(scope: &mut v8::PinScope, source: &str, name: &str) -> bool {
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
pub(crate) static VP_W: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1200);
pub(crate) static VP_H: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(780);
pub(crate) static DPR_BITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Live OS appearance: `true` when the user's effective macOS appearance is Dark. Drives the
/// `prefers-color-scheme` media feature in both the JS `matchMedia` API (via `__prefersDark()`)
/// and, in parallel, the CSS `@media (prefers-color-scheme)` cascade (the `style` crate keeps its
/// own copy, set on the same engine path). Process-global so the engine (any thread) can update it
/// and the JS worker reads the live value on every media-query evaluation.
pub(crate) static COLOR_SCHEME_DARK: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether the loaded document is cross-origin isolated (COOP `same-origin` + COEP `require-corp`),
/// set by the engine from the main document's response headers before the session starts. Read when
/// building each context's environment to set `self.crossOriginIsolated` (workers inherit the page's
/// value). Process-global so the engine (any thread) can set it and the JS worker reads it live.
pub(crate) static CROSS_ORIGIN_ISOLATED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set whether the loaded document is cross-origin isolated. The engine calls this from the main
/// document's response (COOP+COEP) before creating the JS session.
pub fn set_cross_origin_isolated(isolated: bool) {
    CROSS_ORIGIN_ISOLATED.store(isolated, std::sync::atomic::Ordering::Relaxed);
}

/// Read the live cross-origin-isolation flag (used when building the JS environment).
pub(crate) fn cross_origin_isolated() -> bool {
    CROSS_ORIGIN_ISOLATED.load(std::sync::atomic::Ordering::Relaxed)
}

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
pub(crate) fn color_scheme_dark() -> bool {
    COLOR_SCHEME_DARK.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn device_metrics() -> (f64, f64, f64) {
    use std::sync::atomic::Ordering;
    let bits = DPR_BITS.load(Ordering::Relaxed);
    let dpr = if bits == 0 { 2.0 } else { f32::from_bits(bits) };
    (
        VP_W.load(Ordering::Relaxed) as f64,
        VP_H.load(Ordering::Relaxed) as f64,
        dpr as f64,
    )
}

pub(crate) fn install_browser_environment(scope: &mut v8::PinScope, url: &str) {
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
    // Cross-origin isolation flag (from the main document's COOP+COEP), read by the env bootstrap to
    // set `self.crossOriginIsolated`. Workers inherit the page's value.
    {
        let k = v8::String::new(scope, "__crossOriginIsolated").unwrap();
        let b = v8::Boolean::new(scope, cross_origin_isolated());
        global.set(scope, k.into(), b.into());
    }
    eval_internal(scope, BROWSER_ENV_BOOTSTRAP, "<browser-env>");
    // IndexedDB (in-memory) — depends on structuredClone + queueMicrotask from the prior bootstraps.
    eval_internal(scope, INDEXEDDB_BOOTSTRAP, "<indexeddb>");
    // Web Crypto SubtleCrypto (digest + HMAC) — depends on `crypto` from browser-env.
    eval_internal(scope, WEBCRYPTO_BOOTSTRAP, "<webcrypto>");
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

// ---------------------------------------------------------------------------------------------
// Event loop drain + script evaluation against a V8 context.
// ---------------------------------------------------------------------------------------------

/// Maximum number of `__runDueTimers()` iterations when draining the event loop.
pub(crate) const EVENT_LOOP_CAP: usize = 10_000;

/// Wall-clock ceiling for a single [`drain_event_loop`] call before the watchdog forcibly terminates
/// V8 execution. The drain's own time budget is checked only *between* timer/microtask callbacks, so
/// a script that infinite-loops *inside* one callback (e.g. a service worker with `while (true) {}`,
/// used by some WPT tests to pin a worker in the "parsed" state) would otherwise wedge the session
/// thread forever. Set well above any legitimate synchronous callback or network-bound drain (capped
/// at 15s) so it only ever trips on a true in-tick hang.
pub(crate) const DRAIN_WATCHDOG_SECS: u64 = 20;

/// Compile + run a single source string in the current context, capturing console + error.
/// Drains the per-call console buffer of the [`HostState`] into the result. Never panics on a JS
/// error: it is captured into `EvalOutput.error` via a `TryCatch`.
pub(crate) fn eval_source(scope: &mut v8::PinScope, source: &str, name: &str) -> EvalOutput {
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
pub(crate) fn format_exception(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<v8::HandleScope>>,
) -> String {
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

/// Pump every dedicated-worker AND iframe realm one pass, returning `(did_work, total_in_flight)`.
/// Each realm advances its own timers / fetch completions / message deliveries in its own context.
pub(crate) fn pump_realms(scope: &mut v8::PinScope) -> (bool, usize) {
    let (w_did, w_if) = crate::pump_workers(scope);
    let (f_did, f_if) = crate::pump_frames(scope);
    (w_did || f_did, w_if + f_if)
}

/// Drive the event loop to completion (or the time/iteration cap) after page sources have run.
/// Fires the DOM lifecycle events, then alternates V8 microtask checkpoints with the JS
/// `__runDueTimers()` driver. Folds any console output + `__timerErrors` produced during the
/// drain into the last result (matching the prior behavior).
/// Returns whether any timer/microtask actually fired (so `tick` can skip a DOM snapshot when
/// nothing happened).
pub(crate) fn drain_event_loop(
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

    // Arm the in-tick-hang watchdog (see DRAIN_WATCHDOG_SECS): a background thread that terminates
    // V8 execution if this drain doesn't signal completion in time. Disarmed below before the trailing
    // internal evals; if it fired, the pending termination is cancelled so the isolate stays usable.
    let wd_handle = scope.thread_safe_handle();
    let (wd_stop_tx, wd_stop_rx) = std::sync::mpsc::channel::<()>();
    let wd_tripped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wd_tripped_bg = wd_tripped.clone();
    let wd_thread = std::thread::Builder::new()
        .name("js-drain-watchdog".to_string())
        .spawn(move || {
            if wd_stop_rx
                .recv_timeout(std::time::Duration::from_secs(DRAIN_WATCHDOG_SECS))
                .is_err()
            {
                wd_tripped_bg.store(true, std::sync::atomic::Ordering::SeqCst);
                wd_handle.terminate_execution();
            }
        })
        .ok();

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

        let page_in_flight = scope
            .get_current_context()
            .get_slot::<HostState>()
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
        // Run any pending V8 microtasks/promise jobs first.
        scope.perform_microtask_checkpoint();

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
            scope.perform_microtask_checkpoint();
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
                            scope.perform_microtask_checkpoint();
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

    // Final worker drain: a last few passes so any worker task queued right at the end (e.g. a
    // testharness completion message posted to the page) is delivered, ping-ponging with the page
    // loop until both go quiet (bounded).
    {
        let mut rounds = 0usize;
        loop {
            let (w, wif) = pump_realms(scope);
            scope.perform_microtask_checkpoint();
            let p = run_due_timers(scope);
            scope.perform_microtask_checkpoint();
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

    // Disarm the watchdog. If it tripped, the page/worker infinite-looped inside a callback: cancel
    // the V8 termination so the isolate is reusable, and note it so the hang is visible in output.
    let _ = wd_stop_tx.send(());
    if let Some(h) = wd_thread {
        let _ = h.join();
    }
    let wd_fired = wd_tripped.load(std::sync::atomic::Ordering::SeqCst);
    if wd_fired || scope.is_execution_terminating() {
        scope.cancel_terminate_execution();
    }

    // Collect timer/microtask errors recorded JS-side.
    let mut extra: Vec<String> = Vec::new();
    if wd_fired {
        extra.push(format!(
            "⚠ event-loop watchdog: execution exceeded {DRAIN_WATCHDOG_SECS}s and was terminated (in-tick infinite loop?)"
        ));
    }
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
pub(crate) fn run_due_timers(scope: &mut v8::PinScope) -> bool {
    eval_to_bool(
        scope,
        "(typeof __runDueTimers === 'function') && __runDueTimers()",
    )
}

/// Drain all currently-available background fetch completions (non-blocking `try_recv`) and settle
/// each one's JS promise. Returns whether any completion was delivered. No-op when `fetch_rx` is
/// `None` (the no-DOM / eval paths that never start async fetches).
pub(crate) fn resolve_completed_fetches(
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
pub(crate) fn deliver_fetch_completion(scope: &mut v8::PinScope, completion: FetchCompletion) {
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
pub(crate) fn deliver_ws_events(
    scope: &mut v8::PinScope,
    ws_evt_rx: Option<&Receiver<WsEvent>>,
) -> bool {
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
pub(crate) fn deliver_ws_event(scope: &mut v8::PinScope, id: u64, kind: u8, payload: &str) {
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
pub(crate) fn eval_to_bool(scope: &mut v8::PinScope, source: &str) -> bool {
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
pub(crate) fn eval_to_string(scope: &mut v8::PinScope, source: &str) -> Option<String> {
    v8::tc_scope!(let tc, scope);
    let code = v8::String::new(tc, source)?;
    let v = v8::Script::compile(tc, code, None).and_then(|s| s.run(tc))?;
    Some(render_value(tc, v))
}

// ---------------------------------------------------------------------------------------------
// Public API: Runtime, eval_batch, run_with_dom, run_modules.
// ---------------------------------------------------------------------------------------------
