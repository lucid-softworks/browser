use crate::*;

/// Commands sent to the session's runtime thread. Each variant that produces a result carries a
/// one-shot reply channel (a fresh `mpsc` per call) so callers block on exactly their own answer.
pub(crate) enum SessionCmd {
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
        /// `(node_id, top, right, bottom, left)` per positioned box: the CSSOM *used* inset values
        /// in CSS px. Backs `getComputedStyle(el).top` etc. when the element has a box.
        insets: Vec<(usize, f32, f32, f32, f32)>,
        /// `(node_id, top, right, bottom, left)` per box: the CSSOM *used* margin values in CSS px.
        /// Backs `getComputedStyle(el).margin*` so resolved `auto` margins report their used value.
        margins: Vec<(usize, f32, f32, f32, f32)>,
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
    /// Stop the loop; the engine is torn down on the thread it lives on.
    Stop,
}

/// A persistent JS runtime bound to one page. The Lumen engine + realm live for the whole session
/// on a dedicated thread; [`dispatch_event`](Session::dispatch_event) and [`tick`](Session::tick)
/// post commands to that thread and block on the reply, returning a fresh DOM snapshot each time.
/// The session keeps mutating the live document; callers render the returned clone.
/// Per-tab runtime stats, sampled on the session thread and read lock-free by the engine/UI (for the
/// tab tooltip). `heap_bytes` is the allocated JS bytes when available; `cpu_ns` is cumulative time spent actively
/// running JS on this tab's thread — a CPU proxy, since the thread is otherwise blocked on `recv`.
#[derive(Default)]
pub struct TabStats {
    pub heap_bytes: AtomicU64,
    pub cpu_ns: AtomicU64,
}

pub struct Session {
    execution: ExecutionControl,
    last_snapshot: std::sync::Mutex<dom::Document>,
    tx: std::sync::mpsc::Sender<SessionCmd>,
    handle: Option<std::thread::JoinHandle<()>>,
    stats: Arc<TabStats>,
}

impl Session {
    pub fn execution_stopped(&self) -> bool {
        self.execution.is_cancelled()
    }
    fn snapshot(&self) -> dom::Document {
        self.last_snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn remember(&self, doc: &dom::Document) {
        *self.last_snapshot.lock().unwrap_or_else(|e| e.into_inner()) = doc.clone();
    }
    fn receive_snapshot(
        &self,
        receiver: std::sync::mpsc::Receiver<(dom::Document, Vec<String>)>,
    ) -> (dom::Document, Vec<String>) {
        match receiver.recv_timeout(EXECUTION_BUDGET + std::time::Duration::from_secs(2)) {
            Ok((doc, console)) => {
                self.remember(&doc);
                (doc, console)
            }
            Err(_) => {
                self.execution.cancel();
                (self.snapshot(), vec![EXECUTION_STOPPED.to_string()])
            }
        }
    }
    fn stopped_output(&self) -> (EvalOutput, dom::Document, Vec<String>) {
        (
            EvalOutput {
                error: Some(EXECUTION_STOPPED.to_string()),
                ..EvalOutput::default()
            },
            self.snapshot(),
            Vec::new(),
        )
    }
    /// Allocated JS bytes; currently zero because Lumen does not expose this measurement.
    pub fn heap_bytes(&self) -> u64 {
        self.stats.heap_bytes.load(Ordering::Relaxed)
    }
    /// Cumulative active JS time on this tab's thread (ns); the UI samples deltas to show a CPU %.
    pub fn cpu_ns(&self) -> u64 {
        self.stats.cpu_ns.load(Ordering::Relaxed)
    }
}

impl Session {
    /// Spawn the runtime thread, create the engine + context, install the browser environment, run
    /// the initial classic `scripts` in order then the module graph (`entries` + `modules`, via
    /// `fetcher`), drain once, and return the session plus the initial DOM snapshot + per-source
    /// [`EvalOutput`]s (one per classic script, then one per module entry — matching the order
    /// `run_with_dom`/`run_modules` would produce).
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    pub fn new(
        doc: dom::Document,
        scripts: Vec<String>,
        entries: Vec<String>,
        modules: HashMap<String, String>,
        url: &str,
        fetcher: Box<dyn Fn(&str) -> Option<(String, String)> + Send>,
        request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync>,
        ws_connector: WsConnector,
        cookie_getter: Arc<dyn Fn(&str) -> String + Send + Sync>,
        cookie_setter: Arc<dyn Fn(&str, &str) -> bool + Send + Sync>,
        // Layout rects to seed into HostState BEFORE the page's scripts run, so synchronous
        // layout-dependent reads during load see real geometry. `(rects, naturals, insets,
        // margins, scroll_y_css, doc_height_css)` (CSS px). `None` to seed nothing.
        initial_rects: Option<(
            Vec<(usize, f32, f32, f32, f32)>,
            Vec<(usize, f32, f32)>,
            Vec<(usize, f32, f32, f32, f32)>,
            Vec<(usize, f32, f32, f32, f32)>,
            f32,
            f32,
        )>,
    ) -> (Session, dom::Document, Vec<EvalOutput>) {
        let url = url.to_string();
        let fallback = doc.clone();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<SessionCmd>();
        // One-shot channel for the initial snapshot + per-source outputs.
        let (init_tx, init_rx) = std::sync::mpsc::channel::<(dom::Document, Vec<EvalOutput>)>();
        let stats = Arc::new(TabStats::default());
        let stats_thread = stats.clone();

        let host_context = HostContext::current();
        let execution = host_context.new_execution();
        let thread_execution = execution.clone();
        let spawn = std::thread::Builder::new()
            .name("js-session".to_string())
            .stack_size(256 * 1024 * 1024)
            .spawn(move || {
                let _host = host_context.enter();
                // Catch any panic so it never crosses the thread boundary; on panic the init
                // channel is dropped and the caller falls back to an empty snapshot.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    session_thread_main(
                        doc,
                        scripts,
                        entries,
                        modules,
                        url,
                        fetcher,
                        request_fetcher,
                        ws_connector,
                        cookie_getter,
                        cookie_setter,
                        init_tx,
                        cmd_rx,
                        initial_rects,
                        stats_thread,
                        thread_execution,
                    );
                }));
            });

        let handle = match spawn {
            Ok(h) => h,
            Err(e) => {
                return (
                    Session {
                        execution,
                        last_snapshot: std::sync::Mutex::new(fallback.clone()),
                        tx: cmd_tx,
                        handle: None,
                        stats,
                    },
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
        let budget = EXECUTION_BUDGET + std::time::Duration::from_secs(2);
        let (snapshot, outputs) = match init_rx.recv_timeout(budget) {
            Ok(result) => result,
            Err(_) => {
                execution.cancel();
                (
                    fallback,
                    vec![EvalOutput {
                        value: None,
                        console: Vec::new(),
                        error: Some(EXECUTION_STOPPED.to_string()),
                    }],
                )
            }
        };

        (
            Session {
                execution,
                last_snapshot: std::sync::Mutex::new(snapshot.clone()),
                tx: cmd_tx,
                handle: Some(handle),
                stats,
            },
            snapshot,
            outputs,
        )
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
            return (self.snapshot(), vec![EXECUTION_STOPPED.to_string()]);
        }
        self.receive_snapshot(reply_rx)
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
            return (self.snapshot(), vec![EXECUTION_STOPPED.to_string()]);
        }
        self.receive_snapshot(reply_rx)
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
        insets: Vec<(usize, f32, f32, f32, f32)>,
        margins: Vec<(usize, f32, f32, f32, f32)>,
        scroll_y_css: f32,
        doc_height_css: f32,
    ) {
        let _ = self.tx.send(SessionCmd::SetRects {
            rects,
            naturals,
            insets,
            margins,
            scroll_y_css,
            doc_height_css,
        });
    }

    /// Push freshly-rasterized canvas/image RGBA pixels to the worker so `getImageData` returns real
    /// pixels. Fire-and-forget (no reply). `pixels` is `(node_id, width, height, rgba8)` per source.
    pub fn set_canvas_pixels(&self, pixels: Vec<(usize, u32, u32, Vec<u8>)>) {
        let _ = self.tx.send(SessionCmd::SetCanvasPixels { pixels });
    }

    /// Notify the page that the OS appearance (prefers-color-scheme) changed: re-evaluates every
    /// live `MediaQueryList` and fires `change` on those whose `.matches` flipped. The session host
    /// flag is already updated via [`set_color_scheme_dark`]; this just dispatches the JS events and
    /// drains the loop so any DOM mutations the handlers make are reflected.
    pub fn notify_color_scheme_changed(&self) -> (dom::Document, Vec<String>) {
        self.eval_interact("(globalThis.__mediaChanged && globalThis.__mediaChanged())".to_string())
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
        let (out, _doc, _console) = self.eval_full(
            "JSON.stringify((globalThis.__canvasLists||function(){return[]})())".to_string(),
        );
        out.value.unwrap_or_else(|| "[]".to_string())
    }

    /// Return element scroll offsets as `node:left:top;...`. Only elements whose scrolling
    /// accessors have been written are included; zero entries remain present so the engine can
    /// undo a previously-applied translation.
    pub fn element_scroll_offsets(&self) -> String {
        let (out, _doc, _console) = self
            .eval_full("(globalThis.__elementScrollOffsets||function(){return ''})()".to_string());
        out.value.unwrap_or_default()
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
        if self
            .tx
            .send(SessionCmd::Eval {
                source,
                reply: reply_tx,
            })
            .is_err()
        {
            return self.stopped_output();
        }
        match reply_rx.recv_timeout(EXECUTION_BUDGET + std::time::Duration::from_secs(2)) {
            Ok((out, doc, console)) => {
                self.remember(&doc);
                (out, doc, console)
            }
            Err(_) => {
                self.execution.cancel();
                self.stopped_output()
            }
        }
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
    pub fn fire_event_nonbubbling(
        &self,
        node_id: usize,
        kind: &str,
    ) -> (dom::Document, Vec<String>) {
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
        match reply_rx.recv_timeout(EXECUTION_BUDGET + std::time::Duration::from_secs(2)) {
            Ok(result) => {
                if let Some((doc, _)) = &result {
                    self.remember(doc);
                }
                result
            }
            Err(_) => {
                self.execution.cancel();
                None
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.execution.cancel();
        // Interrupt execution before asking the runtime thread to stop and joining it.
        let _ = self.tx.send(SessionCmd::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Own the Lumen engine and its persistent page realm on the session thread.
pub(crate) fn session_thread_main(
    doc: dom::Document,
    scripts: Vec<String>,
    entries: Vec<String>,
    modules: HashMap<String, String>,
    url: String,
    fetcher: Box<dyn Fn(&str) -> Option<(String, String)> + Send>,
    request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync>,
    ws_connector: WsConnector,
    cookie_getter: Arc<dyn Fn(&str) -> String + Send + Sync>,
    cookie_setter: Arc<dyn Fn(&str, &str) -> bool + Send + Sync>,
    init_tx: std::sync::mpsc::Sender<(dom::Document, Vec<EvalOutput>)>,
    cmd_rx: std::sync::mpsc::Receiver<SessionCmd>,
    initial_rects: Option<(
        Vec<(usize, f32, f32, f32, f32)>,
        Vec<(usize, f32, f32)>,
        Vec<(usize, f32, f32, f32, f32)>,
        Vec<(usize, f32, f32, f32, f32)>,
        f32,
        f32,
    )>,
    stats: Arc<TabStats>,
    execution: ExecutionControl,
) {
    // A tab can close while navigation is fetching, before this thread is created.
    // Return its parsed document without attempting to bootstrap a cancelled realm.
    if execution.is_cancelled() {
        let _ = init_tx.send((
            doc,
            vec![EvalOutput {
                error: Some(EXECUTION_STOPPED.to_string()),
                ..EvalOutput::default()
            }],
        ));
        return;
    }
    let shared: SharedDoc = Rc::new(RefCell::new(doc));
    // Background fetch threads deliver completions here; the receiver lives for the whole session
    // (used by every drain — init load and each subsequent command).
    let (fetch_tx, fetch_rx) = std::sync::mpsc::channel::<FetchCompletion>();
    // Background socket threads deliver WebSocket events here; the receiver lives for the whole
    // session and is drained (non-blocking) on every drain pass alongside the fetch channel.
    let (ws_evt_tx, ws_evt_rx) = std::sync::mpsc::channel::<WsEvent>();
    let initial_operation = execution.enter(EXECUTION_BUDGET);
    let mut engine = controlled_engine(&execution);
    let fetcher: Rc<dyn Fn(&str) -> Option<(String, String)>> = Rc::new(move |u: &str| fetcher(u));
    let registry = install_module_loader(&mut engine, modules, fetcher.clone(), &url);
    {
        let scope = engine.ctx();
        // Share one fetcher between the module loader and the JS `fetch()` primitive (as run_modules).
        let state = HostState::with_fetcher(
            Rc::clone(&shared),
            Rc::clone(&fetcher),
            request_fetcher,
            fetch_tx,
            ws_connector,
            ws_evt_tx,
            cookie_getter,
            cookie_setter,
        );
        install_host_state(scope, state);
        install_browser_environment(scope, &url);

        // Register the page context NOW (before the initial scripts run), so a worker created during
        // page load can immediately deliver `postMessage` back into the page.
        crate::set_page_context(
            crate::realms::Realm::current(scope)
                .unwrap_or_else(|_| panic!("page realm must be available")),
        );

        // Seed the engine-computed layout rects BEFORE scripts run, so synchronous
        // getBoundingClientRect / elementFromPoint / caret*FromPoint reads during page load see real
        // geometry (the rect table is otherwise empty until the engine's first post-load push).
        if let Some((rects, naturals, insets, margins, scroll_y_css, doc_height_css)) =
            initial_rects
        {
            let state = host_state(scope);
            {
                let mut map = state.layout_rects.borrow_mut();
                map.clear();
                for (id, x, y, w, h) in rects {
                    map.insert(id, (x, y, w, h));
                }
            }
            {
                let mut ins = state.used_insets.borrow_mut();
                ins.clear();
                for (id, t, r, b, l) in insets {
                    ins.insert(id, (t, r, b, l));
                }
            }
            {
                let mut mar = state.used_margins.borrow_mut();
                mar.clear();
                for (id, t, r, b, l) in margins {
                    mar.insert(id, (t, r, b, l));
                }
            }
            {
                let mut nat = state.image_natural.borrow_mut();
                nat.clear();
                for (id, w, h) in naturals {
                    nat.insert(id, (w, h));
                }
            }
            state.viewport_scroll_y.set(scroll_y_css);
            state.doc_height.set(doc_height_css);
            // The engine's rects reflect the current DOM: mark layout clean so reads don't re-lay-out
            // in-Session until the next script mutation bumps dom_version.
            state.rects_dom_version.set(state.dom_version.get());
        }

        // Run initial classic scripts in order, then the module graph, exactly as the load path.
        let mut results: Vec<EvalOutput> = Vec::with_capacity(scripts.len() + entries.len());
        for source in &scripts {
            results.push(eval_source(scope, source, "<script>"));
        }
        for entry in &entries {
            results.push(run_one_entry(&mut engine, &registry, entry));
        }
        drain_runtime(&mut engine, &mut results, Some(&fetch_rx), Some(&ws_evt_rx));
        let scope = engine.ctx();
        // Load drain done; switch the timer clock to real time so subsequent ticks/events run
        // setInterval/setTimeout/rAF over actual elapsed time.
        eval_internal(
            scope,
            "if (typeof __enterRealtime === 'function') { __enterRealtime(); }",
            "<realtime>",
        );

        // Send the initial snapshot back to Session::new's caller.
        let _ = init_tx.send((shared.borrow().clone(), results));
    }

    drop(initial_operation);
    if execution.is_cancelled() {
        crate::clear_workers();
        crate::clear_frames();
        return;
    }
    // Commands reuse the persistent Lumen page realm.
    for cmd in cmd_rx {
        if execution.is_cancelled() {
            break;
        }
        let _operation = execution.enter(EXECUTION_BUDGET);
        let __cmd_t0 = std::time::Instant::now();
        match cmd {
            SessionCmd::Dispatch {
                node_id,
                kind,
                x,
                y,
                reply,
            } => {
                let scope = engine.ctx();
                let source = format!(
                    "__dispatchSyntheticEvent({}, {}, {{clientX:{}, clientY:{}, button:0}})",
                    node_id,
                    js_string_literal(&kind),
                    x,
                    y,
                );
                // Run the dispatch as one op, then drain the loop, folding console into a result.
                let mut results = vec![eval_source(scope, &source, "<dispatch>")];
                drain_runtime(&mut engine, &mut results, Some(&fetch_rx), Some(&ws_evt_rx));
                let console = results.into_iter().flat_map(|r| r.console).collect();
                let _ = reply.send((shared.borrow().clone(), console));
            }
            SessionCmd::Key {
                node_id,
                key,
                code,
                reply,
            } => {
                let scope = engine.ctx();
                let source = format!(
                    "__handleKeyInput({}, {}, {})",
                    node_id,
                    js_string_literal(&key),
                    js_string_literal(&code),
                );
                let mut results = vec![eval_source(scope, &source, "<key>")];
                drain_runtime(&mut engine, &mut results, Some(&fetch_rx), Some(&ws_evt_rx));
                let console = results.into_iter().flat_map(|r| r.console).collect();
                let _ = reply.send((shared.borrow().clone(), console));
            }
            SessionCmd::Eval { source, reply } => {
                let scope = engine.ctx();
                let mut results = vec![eval_source(scope, &source, "<interact>")];
                let mut first = EvalOutput {
                    value: results[0].value.clone(),
                    error: results[0].error.clone(),
                    console: Vec::new(),
                };
                drain_runtime(&mut engine, &mut results, Some(&fetch_rx), Some(&ws_evt_rx));
                let console = results.into_iter().flat_map(|r| r.console).collect();
                if execution.is_cancelled() {
                    first.value = None;
                    first.error = Some(EXECUTION_STOPPED.to_string());
                }
                let _ = reply.send((first, shared.borrow().clone(), console));
            }
            SessionCmd::Tick { reply } => {
                let mut results = vec![EvalOutput::default()];
                let did_work =
                    drain_runtime(&mut engine, &mut results, Some(&fetch_rx), Some(&ws_evt_rx));
                // Only snapshot+report when something actually ran, so idle ticks are cheap.
                if did_work {
                    let console = results.into_iter().flat_map(|r| r.console).collect();
                    let _ = reply.send(Some((shared.borrow().clone(), console)));
                } else {
                    let _ = reply.send(None);
                }
            }
            SessionCmd::SetRects {
                rects,
                naturals,
                insets,
                margins,
                scroll_y_css,
                doc_height_css,
            } => {
                // Store on HostState (no JS run needed — just update the geometry tables). Re-enter
                // the persistent context to reach the slot. Fire-and-forget: no reply.
                let scope = engine.ctx();
                let state = host_state(scope);
                let mut map = state.layout_rects.borrow_mut();
                map.clear();
                for (id, x, y, w, h) in rects {
                    map.insert(id, (x, y, w, h));
                }
                drop(map);
                let mut ins = state.used_insets.borrow_mut();
                ins.clear();
                for (id, t, r, b, l) in insets {
                    ins.insert(id, (t, r, b, l));
                }
                drop(ins);
                let mut mar = state.used_margins.borrow_mut();
                mar.clear();
                for (id, t, r, b, l) in margins {
                    mar.insert(id, (t, r, b, l));
                }
                drop(mar);
                let mut nat = state.image_natural.borrow_mut();
                nat.clear();
                for (id, w, h) in naturals {
                    nat.insert(id, (w, h));
                }
                drop(nat);
                state.viewport_scroll_y.set(scroll_y_css);
                state.doc_height.set(doc_height_css);
                // Engine rects reflect the current DOM: mark layout clean (see initial-rects path).
                state.rects_dom_version.set(state.dom_version.get());
            }
            SessionCmd::SetCanvasPixels { pixels } => {
                // Store the engine's rasterized RGBA on HostState for getImageData. Re-enter the
                // persistent context to reach the slot. Fire-and-forget: no reply.
                let scope = engine.ctx();
                let state = host_state(scope);
                let mut map = state.canvas_pixels.borrow_mut();
                map.clear();
                for (id, w, h, rgba) in pixels {
                    map.insert(id, (w, h, rgba));
                }
            }
            SessionCmd::Stop => break,
        }
        // Per-tab stats for the tab tooltip: active JS time this command (CPU proxy — the thread is
        // otherwise blocked on recv) + the allocated JS bytes when available. Cheap, sampled once per command.
        stats
            .cpu_ns
            .fetch_add(__cmd_t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        // Lumen does not expose an allocated-byte measurement. Zero means unavailable.
        stats.heap_bytes.store(0, Ordering::Relaxed);
        if execution.is_cancelled() {
            break;
        }
    }
    // Loop ended (Stop or sender dropped). Drop worker realms + the page-context handle BEFORE the
    // engine so the thread-local doesn't outlive the engine it holds handles into.
    crate::clear_workers();
    crate::clear_frames();
    drop(engine);
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    fn session() -> Session {
        Session::new(
            html::parse("<p>keep this document</p>"),
            Vec::new(),
            Vec::new(),
            HashMap::new(),
            "about:blank",
            Box::new(|_| None),
            Arc::new(|_, _, _, _| None),
            Arc::new(|_, _, _| Err("no sockets".into())),
            Arc::new(|_| String::new()),
            Arc::new(|_, _| false),
            None,
        )
        .0
    }
    #[test]
    fn timer_and_promise_deadlines_stop_threads_and_preserve_the_document() {
        for source in [
            "setTimeout(function(){while(true){}},0)",
            "Promise.resolve().then(function(){while(true){}})",
        ] {
            let session = session();
            let original_count = session.snapshot().len();
            let _short = session
                .execution
                .enter(std::time::Duration::from_millis(100));
            let start = std::time::Instant::now();
            let (out, snapshot, _) = session.eval_full(source.to_string());
            assert_eq!(out.error.as_deref(), Some(EXECUTION_STOPPED), "{out:?}");
            assert_eq!(snapshot.len(), original_count);
            let (stopped, retained, _) = session.eval_full("1+1".into());
            assert_eq!(stopped.error.as_deref(), Some(EXECUTION_STOPPED));
            assert_eq!(retained.len(), original_count);
            drop(session);
            assert!(start.elapsed() < std::time::Duration::from_secs(3));
        }
    }
    #[test]
    fn external_cancellation_stops_a_running_session() {
        let session = session();
        let execution = session.execution.clone();
        let cancel = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            execution.cancel();
        });
        let start = std::time::Instant::now();
        let (out, _, _) = session.eval_full("for (;;) {}".into());
        assert_eq!(out.error.as_deref(), Some(EXECUTION_STOPPED));
        drop(session);
        cancel.join().unwrap();
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
    }
}
