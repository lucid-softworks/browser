use crate::*;

/// A JS runtime. Owns one Lumen engine + global context so state persists across `eval` calls.
///
/// The engine is single-thread-bound, so a `Runtime` must be created and used on the same thread.
pub struct Runtime {
    engine: Engine,
    host_context: HostContext,
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
        let mut engine = Engine::new();
        let scope = engine.ctx();
        let state = HostState::new(Rc::new(RefCell::new(dom::Document::new())));
        install_host_state(scope, state);
        let global = scope.global_this();
        install_console_sink(scope, &global);
        eval_internal(scope, TIMERS_BOOTSTRAP, "<timers>");
        Runtime {
            engine,
            host_context: HostContext::current(),
        }
    }

    /// Evaluate a script in the owned context. Globals persist across calls. Never panics on a JS
    /// error — it is captured into `EvalOutput.error`.
    pub fn eval(&mut self, source: &str) -> EvalOutput {
        let _host = self.host_context.enter();
        eval_source(self.engine.ctx(), source, "<eval>")
    }
}

/// Run `sources` in order on a single fresh runtime (so later scripts see earlier globals) and
/// return one [`EvalOutput`] per source.
///
/// Runs on a dedicated worker thread with a generous stack to isolate script execution
/// from the calling thread; the Lumen engine is created on that worker thread
/// (engines are single-thread-bound). A panic on the worker is isolated and surfaced as an error.
pub fn eval_batch(sources: Vec<String>) -> Vec<EvalOutput> {
    let count = sources.len();
    let host_context = HostContext::current();
    let worker = std::thread::Builder::new()
        .name("js-eval".to_string())
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            let _host = host_context.enter();
            let mut rt = Runtime::new();
            let mut results: Vec<EvalOutput> = sources.iter().map(|s| rt.eval(s)).collect();
            // Drive the event loop so timers/microtasks the scripts registered actually run.
            drain_runtime(&mut rt.engine, &mut results, None, None);
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
/// dedicated worker thread; the Lumen engine, the `Rc<RefCell<Document>>`, and all wrappers live on
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
    let host_context = HostContext::current();
    let worker = std::thread::Builder::new()
        .name("js-eval-dom".to_string())
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            let _host = host_context.enter();
            let result: (dom::Document, Vec<EvalOutput>) = {
                let shared: SharedDoc = Rc::new(RefCell::new(doc));
                let mut engine = Engine::new();
                let scope = engine.ctx();
                install_host_state(scope, HostState::new(Rc::clone(&shared)));
                install_browser_environment(scope, &url);
                crate::set_page_context(
                    crate::realms::Realm::current(scope)
                        .unwrap_or_else(|_| panic!("page realm must be available")),
                );
                let mut results: Vec<EvalOutput> = sources
                    .iter()
                    .map(|source| eval_source(scope, source, "<script>"))
                    .collect();
                drain_runtime(&mut engine, &mut results, None, None);
                crate::clear_workers();
                crate::clear_frames();
                // Recover the owned Document. Dropping the engine releases the context (and HostState
                // slot, which holds the only other Rc clone of `shared`), so `try_unwrap` succeeds.
                drop(engine);
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
// ES modules + dynamic import (run_modules). Lumen handles modules natively; we wire resolution.
// ---------------------------------------------------------------------------------------------

/// Drain browser tasks, then report promise rejections after handlers have had a chance to attach.
/// Rejections belong to the engine rather than a particular callback context.
pub(crate) fn drain_runtime(
    engine: &mut Engine,
    results: &mut [EvalOutput],
    fetch_rx: Option<&Receiver<FetchCompletion>>,
    ws_evt_rx: Option<&Receiver<WsEvent>>,
) -> bool {
    let did_work = drain_event_loop(engine.ctx(), results, fetch_rx, ws_evt_rx);
    let rejected = engine.take_unhandled_rejections();
    let had_rejections = !rejected.is_empty();
    if let Some(last) = results.last_mut() {
        for reason in rejected {
            let scope = engine.ctx();
            let stack = scope.member_get(&reason, "stack").ok();
            let text = match stack {
                Some(Value::Str(stack)) => stack.to_string(),
                _ => render_value(scope, reason),
            };
            last.console.push(format!("⚠ Unhandled rejection: {text}"));
        }
    }
    did_work || had_rejections
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_bindings_persist_between_scripts() {
        let mut runtime = Runtime::new();
        let first =
            runtime.eval("let lexicalCounter = 1; const increment = () => ++lexicalCounter;");
        assert!(first.error.is_none(), "{:?}", first.error);
        let second = runtime.eval("increment()");
        assert!(second.error.is_none(), "{:?}", second.error);
        assert_eq!(second.value.as_deref(), Some("2"));
    }

    #[test]
    fn unhandled_rejections_are_reported_after_drain() {
        let outputs = eval_batch(vec![
            "Promise.reject(new Error('rejection-probe'));".to_string()
        ]);
        assert!(
            outputs[0].console.iter().any(
                |line| line.contains("Unhandled rejection") && line.contains("rejection-probe")
            ),
            "{:?}",
            outputs[0]
        );
    }

    #[test]
    fn handled_rejections_are_not_reported() {
        let outputs = eval_batch(vec![
            "Promise.reject(new Error('handled-probe')).catch(() => {});".to_string(),
        ]);
        assert!(
            !outputs[0]
                .console
                .iter()
                .any(|line| line.contains("Unhandled rejection")),
            "{:?}",
            outputs[0]
        );
    }
}
