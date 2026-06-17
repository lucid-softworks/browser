//! JavaScript runtime (Phase: scripting).
//!
//! Wraps the reused `boa_engine` (a pure-Rust JS engine) behind our own small API so the
//! engine can be swapped for a hand-written one later — same pattern as `net`/ureq and
//! `paint`/fontdue. Nothing outside this crate knows Boa exists.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::{
    builtins::promise::PromiseState,
    js_string,
    module::{ModuleLoader, ModuleRequest, Referrer},
    object::{builtins::JsArray, ObjectInitializer},
    property::Attribute,
    Context, JsObject, JsResult, JsValue, Module, NativeFunction, Source,
};

/// A JS execution result: the value rendered as a string (if any) plus any console output
/// captured during execution.
#[derive(Debug, Default, Clone)]
pub struct EvalOutput {
    pub value: Option<String>,
    pub console: Vec<String>,
    pub error: Option<String>,
}

/// A JS runtime. Owns one global context so state persists across `eval` calls.
pub struct Runtime {
    context: Context,
    /// Console lines accumulated by the installed `console` global. Drained on each `eval`.
    console: Rc<RefCell<Vec<String>>>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime {
    pub fn new() -> Self {
        let mut context = Context::default();
        let console = Rc::new(RefCell::new(Vec::new()));
        install_console(&mut context, &console);
        install_timers(&mut context);
        Runtime { context, console }
    }

    /// Evaluate a script in the owned context. State (globals) persists across calls.
    ///
    /// On success `value` is the result rendered as a string (omitted when `undefined`);
    /// on a JS exception `error` holds the message. Any `console.*` output produced during
    /// the call is captured into `console`. Never panics on script errors.
    pub fn eval(&mut self, source: &str) -> EvalOutput {
        // Clear leftover console state (defensive; we drain after every eval anyway).
        self.console.borrow_mut().clear();

        let result = self.context.eval(Source::from_bytes(source));
        let console = std::mem::take(&mut *self.console.borrow_mut());

        match result {
            Ok(value) => {
                let rendered = if value.is_undefined() {
                    None
                } else {
                    Some(render_value(&value, &mut self.context))
                };
                EvalOutput { value: rendered, console, error: None }
            }
            Err(err) => {
                EvalOutput { value: None, console, error: Some(err.to_string()) }
            }
        }
    }
}

/// Run `sources` in order on a single fresh runtime (so later scripts see earlier globals)
/// and return one [`EvalOutput`] per source.
///
/// Executed on a dedicated thread with a large stack. Boa's parser is recursive-descent and
/// recurses very deeply on real-world minified JavaScript; a normal thread stack (e.g. a
/// libdispatch worker's ~512 KiB) overflows and faults — a hardware trap that can't be caught
/// after the fact, so we must give the parser room up front. Running off-thread also isolates
/// ordinary panics inside the engine: a panic terminates this worker and is surfaced as an
/// error here instead of aborting the whole process.
pub fn eval_batch(sources: Vec<String>) -> Vec<EvalOutput> {
    let count = sources.len();
    let worker = std::thread::Builder::new()
        .name("js-eval".to_string())
        // 1 GiB of address space. Boa's recursive-descent parser spends ~17 stack frames per
        // expression-nesting level (≈33 KiB/level in debug builds), so even modestly nested
        // real-world minified JS needs far more than a default thread stack. This is virtual
        // address space — pages commit lazily as the stack grows — so reserving it is cheap.
        .stack_size(1024 * 1024 * 1024)
        .spawn(move || {
            let mut rt = Runtime::new();
            let mut results = sources.iter().map(|s| rt.eval(s)).collect::<Vec<_>>();
            // Drive the event loop so timers/microtasks registered by the scripts actually run.
            let Runtime { context, console } = &mut rt;
            drain_event_loop(context, console, &mut results);
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

/// A shared, mutable handle to the page's DOM. Cloned into every native binding closure so
/// reads and writes from JS hit the one real document tree.
type SharedDoc = Rc<RefCell<dom::Document>>;

/// Run `sources` in order against the live `doc`, returning the (possibly mutated) document
/// and one [`EvalOutput`] per source.
///
/// This is the DOM-aware sibling of [`eval_batch`]. The context is given browser globals
/// (`window`/`self`/`globalThis` aliases, a minimal `location`) and a `document` object wired
/// to `doc`, so scripts like `document.getElementById("x").textContent = "y"` mutate the real
/// tree and the change is visible in the returned document.
///
/// Like `eval_batch`, the parse/eval work runs on a dedicated 1 GiB-stack worker thread (Boa's
/// recursive-descent parser overflows small stacks on real-world minified JS). `doc` and
/// `sources` are both `Send`, so they are moved into the worker; the non-`Send` `Rc`/`RefCell`
/// handle and the Boa `Context` are built *inside* the worker and never cross the boundary.
pub fn run_with_dom(
    doc: dom::Document,
    sources: Vec<String>,
    url: &str,
) -> (dom::Document, Vec<EvalOutput>) {
    let count = sources.len();
    // Move an owned copy of the URL into the worker (the borrow can't cross the thread boundary).
    let url = url.to_string();
    let worker = std::thread::Builder::new()
        .name("js-eval-dom".to_string())
        // See `eval_batch` for why this is 1 GiB of (lazily-committed) address space.
        .stack_size(1024 * 1024 * 1024)
        .spawn(move || {
            let shared: SharedDoc = Rc::new(RefCell::new(doc));

            let mut context = Context::default();
            let console = Rc::new(RefCell::new(Vec::new()));
            install_browser_environment(&mut context, &console, &shared, &url);

            let mut results = Vec::with_capacity(sources.len());
            for source in &sources {
                console.borrow_mut().clear();
                let result = context.eval(Source::from_bytes(source));
                let captured = std::mem::take(&mut *console.borrow_mut());
                match result {
                    Ok(value) => {
                        let rendered = if value.is_undefined() {
                            None
                        } else {
                            Some(render_value(&value, &mut context))
                        };
                        results.push(EvalOutput { value: rendered, console: captured, error: None });
                    }
                    Err(err) => results.push(EvalOutput {
                        value: None,
                        console: captured,
                        error: Some(err.to_string()),
                    }),
                }
            }

            // Now that all page scripts have registered their timers and microtasks, drive the
            // event loop to completion (or the safety cap), folding any console output and timer
            // errors produced during the drain into the results.
            drain_event_loop(&mut context, &console, &mut results);

            // Recover the owned `Document`. Dropping the `Context` first releases every binding
            // closure and element wrapper object that holds an `Rc` clone of `shared`, leaving
            // us as the sole owner so `try_unwrap` succeeds. If anything still holds a reference
            // (it shouldn't), fall back to cloning the inner document so we always return one.
            drop(context);
            let doc = match Rc::try_unwrap(shared) {
                Ok(cell) => cell.into_inner(),
                Err(rc) => rc.borrow().clone(),
            };
            (doc, results)
        });

    match worker {
        Ok(handle) => handle.join().unwrap_or_else(|_| {
            // The worker panicked: we lost the document it owned, so return a fresh empty one.
            let results = vec![
                EvalOutput {
                    value: None,
                    console: Vec::new(),
                    error: Some("script execution aborted (panic in JS engine)".to_string()),
                };
                count.max(1)
            ];
            (dom::Document::new(), results)
        }),
        Err(e) => (
            dom::Document::new(),
            vec![EvalOutput {
                value: None,
                console: Vec::new(),
                error: Some(format!("could not start JS worker thread: {e}")),
            }],
        ),
    }
}

/// Install the full DOM-aware "browser environment" into `context`: console capture, the
/// `window`/`self`/`globalThis` aliases, the DOM-wired `document` (with write-through), the
/// timer/event-loop APIs, and the navigator/location/etc. bootstrap. Shared by both
/// [`run_with_dom`] (classic scripts) and [`run_modules`] (ES modules) so modules see the same
/// `document`/`window` globals page scripts do. Order matters: `install_globals` must precede
/// `install_browser_env` (which overwrites the minimal `location` and patches `document`).
fn install_browser_environment(
    context: &mut Context,
    console: &Rc<RefCell<Vec<String>>>,
    shared: &SharedDoc,
    url: &str,
) {
    install_console(context, console);
    install_globals(context);
    install_document(context, shared);
    install_timers(context);
    install_browser_env(context, url);
}

/// A map-backed ES module loader. The engine has already rewritten every import/export specifier
/// in each module's source to its **canonical absolute URL**, so this loader does no
/// referrer-relative resolution: the `specifier` it receives *is* the canonical URL, and it is
/// looked up directly in `sources`. Parsed [`Module`]s are cached by URL so a module imported from
/// several places is parsed once (and cycles terminate).
///
/// GC soundness: the cached `Module`s are `Trace`-able Boa values, but they live behind an `Rc`
/// owned by the loader (which Boa itself owns via `Context`), not captured into any
/// `NativeFunction::from_closure`. The source `HashMap` holds only `String`s.
struct MapLoader {
    sources: HashMap<String, String>,
    cache: RefCell<HashMap<String, Module>>,
}

impl ModuleLoader for MapLoader {
    async fn load_imported_module(
        self: Rc<Self>,
        _referrer: Referrer,
        request: ModuleRequest,
        context: &RefCell<&mut Context>,
    ) -> JsResult<Module> {
        let key = request.specifier().to_std_string_escaped();
        if let Some(m) = self.cache.borrow().get(&key) {
            return Ok(m.clone());
        }
        let src = match self.sources.get(&key) {
            Some(s) => s.clone(),
            None => {
                return Err(boa_engine::JsNativeError::typ()
                    .with_message(format!("module not found: {key}"))
                    .into());
            }
        };
        let module = {
            let mut ctx = context.borrow_mut();
            Module::parse(Source::from_bytes(src.as_bytes()), None, &mut ctx)?
        };
        self.cache
            .borrow_mut()
            .insert(key, module.clone());
        Ok(module)
    }
}

/// Run the ES module graph for a page. `entries` are the canonical URLs of the entry modules in
/// document order; `modules` maps every canonical module URL to its **already-rewritten** source
/// (every import/export specifier replaced with its canonical URL). Returns the (possibly mutated)
/// document plus one [`EvalOutput`] per entry (console output is folded into the last entry's
/// output, the same way [`run_with_dom`] folds drain output).
///
/// Runs on the same 1 GiB-stack worker thread as [`run_with_dom`] (Boa's recursive-descent parser
/// overflows small stacks on real-world minified JS — Vue is ~400 KB). The browser environment is
/// installed identically via [`install_browser_environment`], so modules see `document`/`window`.
pub fn run_modules(
    doc: dom::Document,
    url: &str,
    entries: Vec<String>,
    modules: HashMap<String, String>,
) -> (dom::Document, Vec<EvalOutput>) {
    let count = entries.len().max(1);
    let url = url.to_string();
    let worker = std::thread::Builder::new()
        .name("js-modules".to_string())
        .stack_size(1024 * 1024 * 1024)
        .spawn(move || {
            let shared: SharedDoc = Rc::new(RefCell::new(doc));

            let loader = Rc::new(MapLoader { sources: modules, cache: RefCell::new(HashMap::new()) });
            let loader_ref = Rc::clone(&loader);
            let mut context = match Context::builder().module_loader(loader).build() {
                Ok(c) => c,
                Err(e) => {
                    let doc = Rc::try_unwrap(shared)
                        .map(RefCell::into_inner)
                        .unwrap_or_else(|rc| rc.borrow().clone());
                    return (
                        doc,
                        vec![EvalOutput {
                            value: None,
                            console: Vec::new(),
                            error: Some(format!("could not build module context: {e}")),
                        }],
                    );
                }
            };

            let console = Rc::new(RefCell::new(Vec::new()));
            install_browser_environment(&mut context, &console, &shared, &url);

            // Parse + kick off load/link/evaluate for every entry module, collecting the returned
            // promises so we can inspect their final state after the event loop drains.
            let mut results: Vec<EvalOutput> = Vec::with_capacity(entries.len());
            let mut promises: Vec<(usize, boa_engine::object::builtins::JsPromise)> = Vec::new();
            for (i, entry) in entries.iter().enumerate() {
                console.borrow_mut().clear();
                // Reuse an already-parsed module if a previous entry imported it; otherwise parse
                // from the source map and seed the cache so transitive imports dedup against it.
                let cached = loader_ref.cache.borrow().get(entry).cloned();
                let parsed = match cached {
                    Some(m) => Ok(m),
                    None => match loader_ref.sources.get(entry).cloned() {
                        Some(src) => Module::parse(Source::from_bytes(src.as_bytes()), None, &mut context),
                        None => {
                            results.push(EvalOutput {
                                value: None,
                                console: std::mem::take(&mut *console.borrow_mut()),
                                error: Some(format!("entry module not found: {entry}")),
                            });
                            continue;
                        }
                    },
                };
                match parsed {
                    Ok(module) => {
                        loader_ref.cache.borrow_mut().insert(entry.clone(), module.clone());
                        let promise = module.load_link_evaluate(&mut context);
                        promises.push((i, promise));
                        results.push(EvalOutput {
                            value: None,
                            console: std::mem::take(&mut *console.borrow_mut()),
                            error: None,
                        });
                    }
                    Err(err) => results.push(EvalOutput {
                        value: None,
                        console: std::mem::take(&mut *console.borrow_mut()),
                        error: Some(err.to_string()),
                    }),
                }
            }

            // Drive the event loop so module top-level await, promise jobs, microtasks, timers,
            // and the DOM lifecycle events all run to completion (or the safety cap).
            drain_event_loop(&mut context, &console, &mut results);

            // Surface any module that finished rejected (load/link/evaluate failure).
            for (i, promise) in promises {
                if let PromiseState::Rejected(reason) = promise.state() {
                    let msg = render_value(&reason, &mut context);
                    if let Some(slot) = results.get_mut(i) {
                        if slot.error.is_none() {
                            slot.error = Some(msg);
                        }
                    }
                }
            }

            drop(context);
            let doc = match Rc::try_unwrap(shared) {
                Ok(cell) => cell.into_inner(),
                Err(rc) => rc.borrow().clone(),
            };
            (doc, results)
        });

    match worker {
        Ok(handle) => handle.join().unwrap_or_else(|_| {
            let results = vec![
                EvalOutput {
                    value: None,
                    console: Vec::new(),
                    error: Some("module execution aborted (panic in JS engine)".to_string()),
                };
                count
            ];
            (dom::Document::new(), results)
        }),
        Err(e) => (
            dom::Document::new(),
            vec![EvalOutput {
                value: None,
                console: Vec::new(),
                error: Some(format!("could not start JS worker thread: {e}")),
            }],
        ),
    }
}

/// Make `window`, `self`, and `globalThis` all refer to the global object, and add a minimal
/// `location` object with an `href` string. After this, `typeof window === "object"`,
/// `window === self`, and `window.foo = 1` sets a global.
fn install_globals(context: &mut Context) {
    let global = context.global_object();
    // `globalThis` already exists in Boa; alias `window`/`self` to the same object.
    context
        .register_global_property(js_string!("window"), global.clone(), Attribute::all())
        .expect("register window global");
    context
        .register_global_property(js_string!("self"), global, Attribute::all())
        .expect("register self global");

    let location = ObjectInitializer::new(context)
        .property(js_string!("href"), js_string!(""), Attribute::all())
        .build();
    context
        .register_global_property(js_string!("location"), location, Attribute::all())
        .expect("register location global");
}

/// Hidden own-property on an element wrapper that stores its [`dom::NodeId`] index, so methods
/// and accessors can recover the node from `this`.
const NODE_KEY: &str = "__node";

/// Read the `NodeId` index stored on an element wrapper `this` value. Returns `None` if `this`
/// is not an element wrapper (e.g. the method was called unbound).
fn node_id_of(this: &JsValue, context: &mut Context) -> Option<dom::NodeId> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(NODE_KEY), context).ok()?;
    let n = v.as_number()?;
    Some(dom::NodeId(n as usize))
}

/// Concatenate every descendant `Text` node under `id`, in document order.
fn text_content(doc: &dom::Document, id: dom::NodeId) -> String {
    let mut out = String::new();
    fn walk(doc: &dom::Document, id: dom::NodeId, out: &mut String) {
        match &doc.get(id).data {
            dom::NodeData::Text(t) => out.push_str(t),
            _ => {
                for &child in &doc.get(id).children {
                    walk(doc, child, out);
                }
            }
        }
    }
    walk(doc, id, &mut out);
    out
}

/// Serialize the children of `id` back to an HTML string (the `innerHTML` of `id`).
///
/// This is a minimal HTML serializer: it emits start/end tags with attributes, text, and
/// comments. It is enough for frameworks that read `container.innerHTML` to recover an in-DOM
/// template (e.g. Vue's `mount` uses the container's innerHTML as the component template), where
/// a text-only serialization would silently drop structural directives like `v-for`/`v-if`.
fn inner_html(doc: &dom::Document, id: dom::NodeId) -> String {
    /// HTML void elements never have an end tag.
    fn is_void(tag: &str) -> bool {
        matches!(
            tag.to_ascii_lowercase().as_str(),
            "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" | "link"
                | "meta" | "param" | "source" | "track" | "wbr"
        )
    }
    fn escape_text(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }
    fn escape_attr(s: &str) -> String {
        s.replace('&', "&amp;").replace('"', "&quot;")
    }
    fn serialize_node(doc: &dom::Document, id: dom::NodeId, out: &mut String) {
        match &doc.get(id).data {
            dom::NodeData::Text(t) => out.push_str(&escape_text(t)),
            dom::NodeData::Comment(c) => {
                out.push_str("<!--");
                out.push_str(c);
                out.push_str("-->");
            }
            dom::NodeData::Element(e) => {
                out.push('<');
                out.push_str(&e.tag);
                for (k, v) in &e.attrs {
                    out.push(' ');
                    out.push_str(k);
                    out.push_str("=\"");
                    out.push_str(&escape_attr(v));
                    out.push('"');
                }
                out.push('>');
                if !is_void(&e.tag) {
                    for &child in &doc.get(id).children {
                        serialize_node(doc, child, out);
                    }
                    out.push_str("</");
                    out.push_str(&e.tag);
                    out.push('>');
                }
            }
            dom::NodeData::Document => {
                for &child in &doc.get(id).children {
                    serialize_node(doc, child, out);
                }
            }
        }
    }
    let mut out = String::new();
    for &child in &doc.get(id).children {
        serialize_node(doc, child, &mut out);
    }
    out
}

/// Replace all children of `id` with a single `Text` node holding `text`.
fn set_text_content(doc: &mut dom::Document, id: dom::NodeId, text: &str) {
    // Detach existing children (orphan them; the arena keeps the slots, which is fine).
    let old: Vec<dom::NodeId> = std::mem::take(&mut doc.get_mut(id).children);
    for child in old {
        doc.get_mut(child).parent = None;
    }
    doc.append_child(id, dom::NodeData::Text(text.to_string()));
}

/// Depth-first search for the first element whose tag equals `tag` (ASCII case-insensitive).
fn find_by_tag(doc: &dom::Document, root: dom::NodeId, tag: &str) -> Option<dom::NodeId> {
    if let dom::NodeData::Element(e) = &doc.get(root).data {
        if e.tag.eq_ignore_ascii_case(tag) {
            return Some(root);
        }
    }
    for &child in &doc.get(root).children {
        if let Some(found) = find_by_tag(doc, child, tag) {
            return Some(found);
        }
    }
    None
}

/// Collect every element matching `tag` (ASCII case-insensitive), document order.
fn collect_by_tag(doc: &dom::Document, root: dom::NodeId, tag: &str, out: &mut Vec<dom::NodeId>) {
    if let dom::NodeData::Element(e) = &doc.get(root).data {
        if e.tag.eq_ignore_ascii_case(tag) {
            out.push(root);
        }
    }
    let children = doc.get(root).children.clone();
    for child in children {
        collect_by_tag(doc, child, tag, out);
    }
}

/// Depth-first search for the first element with `id` equal to `id`.
fn find_by_id(doc: &dom::Document, root: dom::NodeId, id: &str) -> Option<dom::NodeId> {
    if let dom::NodeData::Element(e) = &doc.get(root).data {
        if e.id() == Some(id) {
            return Some(root);
        }
    }
    for &child in &doc.get(root).children {
        if let Some(found) = find_by_id(doc, child, id) {
            return Some(found);
        }
    }
    None
}

/// A single compound selector, e.g. `div.foo#bar` → tag=Some("div"), id=Some("bar"),
/// classes=["foo"]. `tag` of `*` (or none) matches any element.
#[derive(Debug, Default, Clone)]
struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    /// True if at least one parseable simple piece was present (otherwise the compound is empty
    /// and should not match everything).
    any: bool,
}

impl Compound {
    fn matches(&self, doc: &dom::Document, node: dom::NodeId) -> bool {
        let e = match &doc.get(node).data {
            dom::NodeData::Element(e) => e,
            _ => return false,
        };
        if let Some(tag) = &self.tag {
            if tag != "*" && !e.tag.eq_ignore_ascii_case(tag) {
                return false;
            }
        }
        if let Some(id) = &self.id {
            if e.id() != Some(id.as_str()) {
                return false;
            }
        }
        for c in &self.classes {
            if !e.classes().any(|x| x == c) {
                return false;
            }
        }
        true
    }
}

/// Parse a single compound selector (no combinators). Recognizes `tag`, `*`, `.class`, `#id`,
/// and `[attr]`/`[attr=val]` (attribute presence/equality, value quotes stripped). Unknown
/// pieces (pseudo-classes etc.) are ignored, which is a pragmatic over-match rather than a throw.
fn parse_compound(s: &str) -> Option<Compound> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut c = Compound::default();
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        match ch {
            '.' | '#' => {
                i += 1;
                let start = i;
                while i < bytes.len() && !matches!(bytes[i], '.' | '#' | '[' | ':') {
                    i += 1;
                }
                let name: String = bytes[start..i].iter().collect();
                if name.is_empty() {
                    return None;
                }
                if ch == '.' {
                    c.classes.push(name);
                } else {
                    c.id = Some(name);
                }
                c.any = true;
            }
            '[' => {
                // Skip an attribute selector; we ignore its constraint (over-match) but must not
                // choke on it. Consume up to the matching ']'.
                while i < bytes.len() && bytes[i] != ']' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
                c.any = true;
            }
            ':' => {
                // Pseudo-class/element: skip the name (and any (...) group); ignore it.
                i += 1;
                if i < bytes.len() && bytes[i] == ':' {
                    i += 1;
                }
                while i < bytes.len() && !matches!(bytes[i], '.' | '#' | '[' | ':') {
                    if bytes[i] == '(' {
                        let mut depth = 1;
                        i += 1;
                        while i < bytes.len() && depth > 0 {
                            match bytes[i] {
                                '(' => depth += 1,
                                ')' => depth -= 1,
                                _ => {}
                            }
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
                c.any = true;
            }
            _ => {
                // A type/universal selector at the start.
                let start = i;
                while i < bytes.len() && !matches!(bytes[i], '.' | '#' | '[' | ':') {
                    i += 1;
                }
                let tag: String = bytes[start..i].iter().collect();
                let tag = tag.trim().to_string();
                if !tag.is_empty() {
                    c.tag = Some(tag);
                    c.any = true;
                }
            }
        }
    }
    if c.any {
        Some(c)
    } else {
        None
    }
}

/// A complex selector: a chain of compounds joined by descendant combinators (whitespace).
/// `a b c` → match a `c` that has a `b` ancestor that has an `a` ancestor. We treat `>` like a
/// descendant combinator (over-match) for simplicity, which is acceptable for our purposes.
fn parse_complex(s: &str) -> Option<Vec<Compound>> {
    // Normalize `>` `+` `~` combinators to spaces (over-match; we only do descendant matching).
    let normalized: String = s
        .chars()
        .map(|c| if matches!(c, '>' | '+' | '~') { ' ' } else { c })
        .collect();
    let parts: Vec<Compound> = normalized
        .split_whitespace()
        .filter_map(parse_compound)
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

/// Does `node` match the complex selector `chain` (last compound matches `node`, earlier
/// compounds match successive ancestors in order)?
fn matches_complex(doc: &dom::Document, node: dom::NodeId, chain: &[Compound]) -> bool {
    if chain.is_empty() {
        return false;
    }
    let last = &chain[chain.len() - 1];
    if !last.matches(doc, node) {
        return false;
    }
    // Walk ancestors, greedily satisfying the remaining (earlier) compounds.
    let mut remaining = &chain[..chain.len() - 1];
    let mut cur = doc.get(node).parent;
    while !remaining.is_empty() {
        let want = &remaining[remaining.len() - 1];
        match cur {
            None => return false,
            Some(p) => {
                if want.matches(doc, p) {
                    remaining = &remaining[..remaining.len() - 1];
                }
                cur = doc.get(p).parent;
            }
        }
    }
    true
}

/// Collect every node matching any of the comma-separated selector groups, in document order.
fn query_selector_all(doc: &dom::Document, sel: &str) -> Vec<dom::NodeId> {
    let groups: Vec<Vec<Compound>> = sel.split(',').filter_map(parse_complex).collect();
    if groups.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    fn walk(doc: &dom::Document, node: dom::NodeId, groups: &[Vec<Compound>], out: &mut Vec<dom::NodeId>) {
        if matches!(doc.get(node).data, dom::NodeData::Element(_))
            && groups.iter().any(|g| matches_complex(doc, node, g))
        {
            out.push(node);
        }
        let children = doc.get(node).children.clone();
        for child in children {
            walk(doc, child, groups, out);
        }
    }
    walk(doc, doc.root(), &groups, &mut out);
    out
}

/// First node matching `sel`, or none.
fn query_selector(doc: &dom::Document, sel: &str) -> Option<dom::NodeId> {
    query_selector_all(doc, sel).into_iter().next()
}

/// Like [`query_selector_all`] but scoped to the subtree under `root` (excluding `root` itself),
/// used for element-level `querySelector`/`querySelectorAll`. Ancestor matching for descendant
/// combinators still consults real ancestors (above `root`), matching browser semantics closely
/// enough for our purposes.
fn query_within(doc: &dom::Document, root: dom::NodeId, sel: &str) -> Vec<dom::NodeId> {
    let groups: Vec<Vec<Compound>> = sel.split(',').filter_map(parse_complex).collect();
    let mut out = Vec::new();
    if groups.is_empty() {
        return out;
    }
    fn walk(doc: &dom::Document, node: dom::NodeId, groups: &[Vec<Compound>], out: &mut Vec<dom::NodeId>) {
        if matches!(doc.get(node).data, dom::NodeData::Element(_))
            && groups.iter().any(|g| matches_complex(doc, node, g))
        {
            out.push(node);
        }
        let children = doc.get(node).children.clone();
        for child in children {
            walk(doc, child, groups, out);
        }
    }
    let children = doc.get(root).children.clone();
    for child in children {
        walk(doc, child, &groups, &mut out);
    }
    out
}

/// Build a JS element wrapper object for `id`: a plain object carrying the hidden `__node`
/// index, with accessors (`textContent`, `tagName`, `nodeName`, `id`, `className`, `innerHTML`)
/// and methods (`getAttribute`, `setAttribute`, `appendChild`) that operate on the shared doc.
fn make_element(id: dom::NodeId, doc: &SharedDoc, context: &mut Context) -> JsObject {
    // Accessor functions take `&Realm`; clone it up front so we don't borrow `context` while
    // also handing it to `ObjectInitializer`.
    let realm = context.realm().clone();

    // --- textContent: get concatenates descendant text; set replaces children with one text. ---
    let tc_get = {
        let doc = Rc::clone(doc);
        // SAFETY: captures only a `SharedDoc` (`Rc<RefCell<dom::Document>>`), which holds no
        // GC-traceable values. Per `from_closure`'s contract this is sound — see `console`.
        unsafe {
            NativeFunction::from_closure(move |this, _args, ctx| {
                let s = node_id_of(this, ctx)
                    .map(|n| text_content(&doc.borrow(), n))
                    .unwrap_or_default();
                Ok(JsValue::from(js_string!(s)))
            })
        }
        .to_js_function(&realm)
    };
    let tc_set = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, args, ctx| {
                if let Some(n) = node_id_of(this, ctx) {
                    let text = args
                        .first()
                        .map(|a| render_value(a, ctx))
                        .unwrap_or_default();
                    set_text_content(&mut doc.borrow_mut(), n, &text);
                }
                Ok(JsValue::undefined())
            })
        }
        .to_js_function(&realm)
    };

    // --- innerHTML: get serializes children back to HTML markup (tags + attrs + text), so code
    // that reads `el.innerHTML` as a template (e.g. Vue's `mount` uses the mount container's
    // innerHTML as the component template) recovers structural directives like `v-for`/`v-if`
    // instead of a flattened text run. set replaces with a single text node. ---
    let html_get = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, _args, ctx| {
                let s = node_id_of(this, ctx)
                    .map(|n| inner_html(&doc.borrow(), n))
                    .unwrap_or_default();
                Ok(JsValue::from(js_string!(s)))
            })
        }
        .to_js_function(&realm)
    };
    let html_set = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, args, ctx| {
                if let Some(n) = node_id_of(this, ctx) {
                    let text = args
                        .first()
                        .map(|a| render_value(a, ctx))
                        .unwrap_or_default();
                    set_text_content(&mut doc.borrow_mut(), n, &text);
                }
                Ok(JsValue::undefined())
            })
        }
        .to_js_function(&realm)
    };

    // --- tagName / nodeName: uppercased tag name. ---
    let tag_get = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, _args, ctx| {
                let s = node_id_of(this, ctx)
                    .and_then(|n| match &doc.borrow().get(n).data {
                        dom::NodeData::Element(e) => Some(e.tag.to_ascii_uppercase()),
                        _ => None,
                    })
                    .unwrap_or_default();
                Ok(JsValue::from(js_string!(s)))
            })
        }
        .to_js_function(&realm)
    };
    let nodename_get = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, _args, ctx| {
                let s = node_id_of(this, ctx)
                    .and_then(|n| match &doc.borrow().get(n).data {
                        dom::NodeData::Element(e) => Some(e.tag.to_ascii_uppercase()),
                        _ => None,
                    })
                    .unwrap_or_default();
                Ok(JsValue::from(js_string!(s)))
            })
        }
        .to_js_function(&realm)
    };

    // --- id (get/set) ---
    let id_get = attr_getter(doc, &realm, "id");
    let id_set = attr_setter(doc, &realm, "id");

    // --- className (get/set) maps to the `class` attribute. ---
    let class_get = attr_getter(doc, &realm, "class");
    let class_set = attr_setter(doc, &realm, "class");

    // --- getAttribute(name) / setAttribute(name, value) ---
    let get_attr = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, args, ctx| {
                let name = args.first().map(|a| render_value(a, ctx)).unwrap_or_default();
                let val = node_id_of(this, ctx).and_then(|n| match &doc.borrow().get(n).data {
                    dom::NodeData::Element(e) => e.attrs.get(&name).cloned(),
                    _ => None,
                });
                Ok(match val {
                    Some(v) => JsValue::from(js_string!(v)),
                    None => JsValue::null(),
                })
            })
        }
    };
    let set_attr = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, args, ctx| {
                let name = args.first().map(|a| render_value(a, ctx)).unwrap_or_default();
                let value = args.get(1).map(|a| render_value(a, ctx)).unwrap_or_default();
                if let Some(n) = node_id_of(this, ctx) {
                    if let dom::NodeData::Element(e) = &mut doc.borrow_mut().get_mut(n).data {
                        e.attrs.insert(name, value);
                    }
                }
                Ok(JsValue::undefined())
            })
        }
    };

    // --- removeAttribute / hasAttribute ---
    let remove_attr = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, args, ctx| {
                let name = args.first().map(|a| render_value(a, ctx)).unwrap_or_default();
                if let Some(n) = node_id_of(this, ctx) {
                    if let dom::NodeData::Element(e) = &mut doc.borrow_mut().get_mut(n).data {
                        e.attrs.remove(&name);
                    }
                }
                Ok(JsValue::undefined())
            })
        }
    };
    let has_attr = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, args, ctx| {
                let name = args.first().map(|a| render_value(a, ctx)).unwrap_or_default();
                let present = node_id_of(this, ctx)
                    .map(|n| match &doc.borrow().get(n).data {
                        dom::NodeData::Element(e) => e.attrs.contains_key(&name),
                        _ => false,
                    })
                    .unwrap_or(false);
                Ok(JsValue::from(present))
            })
        }
    };

    // --- appendChild(child): reparent `child`'s node under `this`, return the child. ---
    let append_child = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, args, ctx| {
                let parent = node_id_of(this, ctx);
                let child_val = args.first().cloned().unwrap_or_else(JsValue::null);
                let child = node_id_of(&child_val, ctx);
                if let (Some(parent), Some(child)) = (parent, child) {
                    let mut d = doc.borrow_mut();
                    // Unlink from any previous parent.
                    if let Some(old_parent) = d.get(child).parent {
                        d.get_mut(old_parent).children.retain(|&c| c != child);
                    }
                    d.get_mut(child).parent = Some(parent);
                    d.get_mut(parent).children.push(child);
                }
                Ok(child_val)
            })
        }
    };

    // --- removeChild(child): unlink `child` from `this`, return it. ---
    let remove_child = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, args, ctx| {
                let parent = node_id_of(this, ctx);
                let child_val = args.first().cloned().unwrap_or_else(JsValue::null);
                let child = node_id_of(&child_val, ctx);
                if let (Some(parent), Some(child)) = (parent, child) {
                    let mut d = doc.borrow_mut();
                    d.get_mut(parent).children.retain(|&c| c != child);
                    if d.get(child).parent == Some(parent) {
                        d.get_mut(child).parent = None;
                    }
                }
                Ok(child_val)
            })
        }
    };

    // --- insertBefore(newNode, refNode): insert before refNode (or append if null). ---
    let insert_before = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, args, ctx| {
                let parent = node_id_of(this, ctx);
                let new_val = args.first().cloned().unwrap_or_else(JsValue::null);
                let new_node = node_id_of(&new_val, ctx);
                let ref_node = args.get(1).and_then(|v| node_id_of(v, ctx));
                if let (Some(parent), Some(new_node)) = (parent, new_node) {
                    let mut d = doc.borrow_mut();
                    if let Some(old) = d.get(new_node).parent {
                        d.get_mut(old).children.retain(|&c| c != new_node);
                    }
                    d.get_mut(new_node).parent = Some(parent);
                    let pos = ref_node
                        .and_then(|r| d.get(parent).children.iter().position(|&c| c == r));
                    match pos {
                        Some(i) => d.get_mut(parent).children.insert(i, new_node),
                        None => d.get_mut(parent).children.push(new_node),
                    }
                }
                Ok(new_val)
            })
        }
    };

    // --- contains(node): is `node` a descendant-or-self of `this`? ---
    let contains_fn = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, args, ctx| {
                let me = node_id_of(this, ctx);
                let other = args.first().and_then(|v| node_id_of(v, ctx));
                let result = match (me, other) {
                    (Some(me), Some(mut cur)) => {
                        let d = doc.borrow();
                        loop {
                            if cur == me {
                                break true;
                            }
                            match d.get(cur).parent {
                                Some(p) => cur = p,
                                None => break false,
                            }
                        }
                    }
                    _ => false,
                };
                Ok(JsValue::from(result))
            })
        }
    };

    // --- matches(sel): does `this` match the selector? ---
    let matches_fn = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, args, ctx| {
                let sel = args.first().map(|a| render_value(a, ctx)).unwrap_or_default();
                let result = node_id_of(this, ctx)
                    .map(|n| {
                        let d = doc.borrow();
                        sel.split(',')
                            .filter_map(parse_complex)
                            .any(|g| matches_complex(&d, n, &g))
                    })
                    .unwrap_or(false);
                Ok(JsValue::from(result))
            })
        }
    };

    // --- closest(sel): nearest ancestor-or-self matching the selector. ---
    let closest_fn = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, args, ctx| {
                let sel = args.first().map(|a| render_value(a, ctx)).unwrap_or_default();
                let found = node_id_of(this, ctx).and_then(|start| {
                    let groups: Vec<Vec<Compound>> =
                        sel.split(',').filter_map(parse_complex).collect();
                    let d = doc.borrow();
                    let mut cur = Some(start);
                    while let Some(n) = cur {
                        if matches!(d.get(n).data, dom::NodeData::Element(_))
                            && groups.iter().any(|g| matches_complex(&d, n, g))
                        {
                            return Some(n);
                        }
                        cur = d.get(n).parent;
                    }
                    None
                });
                Ok(element_or_null(found, &doc, ctx))
            })
        }
    };

    // --- scoped querySelector / querySelectorAll (search within `this`'s subtree). ---
    let el_query = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, args, ctx| {
                let sel = args.first().map(|a| render_value(a, ctx)).unwrap_or_default();
                let found = node_id_of(this, ctx).and_then(|root| {
                    let d = doc.borrow();
                    query_within(&d, root, &sel).into_iter().next()
                });
                Ok(element_or_null(found, &doc, ctx))
            })
        }
    };
    let el_query_all = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, args, ctx| {
                let sel = args.first().map(|a| render_value(a, ctx)).unwrap_or_default();
                let ids = node_id_of(this, ctx)
                    .map(|root| {
                        let d = doc.borrow();
                        query_within(&d, root, &sel)
                    })
                    .unwrap_or_default();
                let items: Vec<JsValue> =
                    ids.into_iter().map(|n| JsValue::from(make_element(n, &doc, ctx))).collect();
                Ok(JsValue::from(JsArray::from_iter(items, ctx)))
            })
        }
    };
    let el_get_by_tag = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, args, ctx| {
                let tag = args.first().map(|a| render_value(a, ctx)).unwrap_or_default();
                let mut ids = Vec::new();
                if let Some(root) = node_id_of(this, ctx) {
                    let d = doc.borrow();
                    // collect_by_tag includes `root` itself; getElementsByTagName excludes self,
                    // so collect over the children only.
                    for &child in &d.get(root).children {
                        collect_by_tag(&d, child, &tag, &mut ids);
                    }
                }
                let items: Vec<JsValue> =
                    ids.into_iter().map(|n| JsValue::from(make_element(n, &doc, ctx))).collect();
                Ok(JsValue::from(JsArray::from_iter(items, ctx)))
            })
        }
    };

    // --- navigation accessors: parentNode / parentElement / children / childNodes /
    // firstChild / lastChild / nextSibling / previousSibling / *ElementSibling. ---
    let parent_get = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, _args, ctx| {
                let found = node_id_of(this, ctx).and_then(|n| doc.borrow().get(n).parent);
                // Don't expose the Document root as a parentElement.
                Ok(element_or_null(found, &doc, ctx))
            })
        }
        .to_js_function(&realm)
    };
    let children_get = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, _args, ctx| {
                let ids: Vec<dom::NodeId> = node_id_of(this, ctx)
                    .map(|n| {
                        let d = doc.borrow();
                        d.get(n)
                            .children
                            .iter()
                            .copied()
                            .filter(|&c| matches!(d.get(c).data, dom::NodeData::Element(_)))
                            .collect()
                    })
                    .unwrap_or_default();
                let items: Vec<JsValue> =
                    ids.into_iter().map(|n| JsValue::from(make_element(n, &doc, ctx))).collect();
                Ok(JsValue::from(JsArray::from_iter(items, ctx)))
            })
        }
        .to_js_function(&realm)
    };
    let child_nodes_get = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, _args, ctx| {
                let ids: Vec<dom::NodeId> = node_id_of(this, ctx)
                    .map(|n| doc.borrow().get(n).children.clone())
                    .unwrap_or_default();
                let items: Vec<JsValue> =
                    ids.into_iter().map(|n| JsValue::from(make_element(n, &doc, ctx))).collect();
                Ok(JsValue::from(JsArray::from_iter(items, ctx)))
            })
        }
        .to_js_function(&realm)
    };
    let first_child_get = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, _args, ctx| {
                let found = node_id_of(this, ctx)
                    .and_then(|n| doc.borrow().get(n).children.first().copied());
                Ok(element_or_null(found, &doc, ctx))
            })
        }
        .to_js_function(&realm)
    };
    let last_child_get = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, _args, ctx| {
                let found = node_id_of(this, ctx)
                    .and_then(|n| doc.borrow().get(n).children.last().copied());
                Ok(element_or_null(found, &doc, ctx))
            })
        }
        .to_js_function(&realm)
    };
    let first_el_child_get = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, _args, ctx| {
                let found = node_id_of(this, ctx).and_then(|n| {
                    let d = doc.borrow();
                    d.get(n)
                        .children
                        .iter()
                        .copied()
                        .find(|&c| matches!(d.get(c).data, dom::NodeData::Element(_)))
                });
                Ok(element_or_null(found, &doc, ctx))
            })
        }
        .to_js_function(&realm)
    };
    let sibling_get = |doc: &SharedDoc, realm: &boa_engine::realm::Realm, next: bool, element_only: bool| {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |this, _args, ctx| {
                let found = node_id_of(this, ctx).and_then(|n| {
                    let d = doc.borrow();
                    let parent = d.get(n).parent?;
                    let sibs = &d.get(parent).children;
                    let idx = sibs.iter().position(|&c| c == n)?;
                    let mut iter_pos = idx;
                    loop {
                        let cand = if next {
                            if iter_pos + 1 >= sibs.len() {
                                return None;
                            }
                            iter_pos += 1;
                            sibs[iter_pos]
                        } else {
                            if iter_pos == 0 {
                                return None;
                            }
                            iter_pos -= 1;
                            sibs[iter_pos]
                        };
                        if !element_only || matches!(d.get(cand).data, dom::NodeData::Element(_)) {
                            return Some(cand);
                        }
                    }
                });
                Ok(element_or_null(found, &doc, ctx))
            })
        }
        .to_js_function(realm)
    };
    let next_sibling_get = sibling_get(doc, &realm, true, false);
    let prev_sibling_get = sibling_get(doc, &realm, false, false);
    let next_el_sibling_get = sibling_get(doc, &realm, true, true);
    let prev_el_sibling_get = sibling_get(doc, &realm, false, true);

    let attr = Attribute::all();
    let obj = ObjectInitializer::new(context)
        .function(get_attr, js_string!("getAttribute"), 1)
        .function(set_attr, js_string!("setAttribute"), 2)
        .function(remove_attr, js_string!("removeAttribute"), 1)
        .function(has_attr, js_string!("hasAttribute"), 1)
        .function(append_child, js_string!("appendChild"), 1)
        .function(remove_child, js_string!("removeChild"), 1)
        .function(insert_before, js_string!("insertBefore"), 2)
        .function(contains_fn, js_string!("contains"), 1)
        .function(matches_fn, js_string!("matches"), 1)
        .function(closest_fn, js_string!("closest"), 1)
        .function(el_query, js_string!("querySelector"), 1)
        .function(el_query_all, js_string!("querySelectorAll"), 1)
        .function(el_get_by_tag, js_string!("getElementsByTagName"), 1)
        .accessor(js_string!("textContent"), Some(tc_get), Some(tc_set), attr)
        .accessor(js_string!("innerHTML"), Some(html_get), Some(html_set), attr)
        .accessor(js_string!("tagName"), Some(tag_get), None, attr)
        .accessor(js_string!("nodeName"), Some(nodename_get), None, attr)
        .accessor(js_string!("id"), Some(id_get), Some(id_set), attr)
        .accessor(js_string!("className"), Some(class_get), Some(class_set), attr)
        .accessor(js_string!("parentNode"), Some(parent_get.clone()), None, attr)
        .accessor(js_string!("parentElement"), Some(parent_get), None, attr)
        .accessor(js_string!("children"), Some(children_get), None, attr)
        .accessor(js_string!("childNodes"), Some(child_nodes_get), None, attr)
        .accessor(js_string!("firstChild"), Some(first_child_get), None, attr)
        .accessor(js_string!("lastChild"), Some(last_child_get), None, attr)
        .accessor(js_string!("firstElementChild"), Some(first_el_child_get), None, attr)
        .accessor(js_string!("nextSibling"), Some(next_sibling_get), None, attr)
        .accessor(js_string!("previousSibling"), Some(prev_sibling_get), None, attr)
        .accessor(js_string!("nextElementSibling"), Some(next_el_sibling_get), None, attr)
        .accessor(js_string!("previousElementSibling"), Some(prev_el_sibling_get), None, attr)
        .build();

    // Store the node index as a non-enumerable, non-writable own property.
    obj.create_data_property_or_throw(js_string!(NODE_KEY), JsValue::from(id.0 as i32), context)
        .expect("store __node on element wrapper");
    obj
}

/// Build a getter for the element attribute `name` (returns "" when absent).
fn attr_getter(doc: &SharedDoc, realm: &boa_engine::realm::Realm, name: &'static str) -> boa_engine::object::builtins::JsFunction {
    let doc = Rc::clone(doc);
    unsafe {
        NativeFunction::from_closure(move |this, _args, ctx| {
            let s = node_id_of(this, ctx)
                .and_then(|n| match &doc.borrow().get(n).data {
                    dom::NodeData::Element(e) => e.attrs.get(name).cloned(),
                    _ => None,
                })
                .unwrap_or_default();
            Ok(JsValue::from(js_string!(s)))
        })
    }
    .to_js_function(realm)
}

/// Build a setter for the element attribute `name`.
fn attr_setter(doc: &SharedDoc, realm: &boa_engine::realm::Realm, name: &'static str) -> boa_engine::object::builtins::JsFunction {
    let doc = Rc::clone(doc);
    unsafe {
        NativeFunction::from_closure(move |this, args, ctx| {
            let value = args.first().map(|a| render_value(a, ctx)).unwrap_or_default();
            if let Some(n) = node_id_of(this, ctx) {
                if let dom::NodeData::Element(e) = &mut doc.borrow_mut().get_mut(n).data {
                    e.attrs.insert(name.to_string(), value);
                }
            }
            Ok(JsValue::undefined())
        })
    }
    .to_js_function(realm)
}

/// Build a JS value for an optional element node: a wrapper object, or `null`.
fn element_or_null(id: Option<dom::NodeId>, doc: &SharedDoc, context: &mut Context) -> JsValue {
    match id {
        Some(id) => JsValue::from(make_element(id, doc, context)),
        None => JsValue::null(),
    }
}

/// Install the `document` global wired to the shared DOM: `getElementById`, `getElementsByTagName`,
/// `querySelector`, `querySelectorAll`, `createElement` (methods), and `title`, `body`,
/// `documentElement` (accessors).
fn install_document(context: &mut Context, doc: &SharedDoc) {
    let realm = context.realm().clone();

    let get_by_id = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |_this, args, ctx| {
                let id = args.first().map(|a| render_value(a, ctx)).unwrap_or_default();
                let found = find_by_id(&doc.borrow(), doc.borrow().root(), &id);
                Ok(element_or_null(found, &doc, ctx))
            })
        }
    };

    let get_by_tag = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |_this, args, ctx| {
                let tag = args.first().map(|a| render_value(a, ctx)).unwrap_or_default();
                let mut ids = Vec::new();
                {
                    let d = doc.borrow();
                    collect_by_tag(&d, d.root(), &tag, &mut ids);
                }
                let items: Vec<JsValue> =
                    ids.into_iter().map(|n| JsValue::from(make_element(n, &doc, ctx))).collect();
                let arr = JsArray::from_iter(items, ctx);
                Ok(JsValue::from(arr))
            })
        }
    };

    let query = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |_this, args, ctx| {
                let sel = args.first().map(|a| render_value(a, ctx)).unwrap_or_default();
                let found = query_selector(&doc.borrow(), &sel);
                Ok(element_or_null(found, &doc, ctx))
            })
        }
    };

    let query_all = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |_this, args, ctx| {
                let sel = args.first().map(|a| render_value(a, ctx)).unwrap_or_default();
                let ids = {
                    let d = doc.borrow();
                    query_selector_all(&d, &sel)
                };
                let items: Vec<JsValue> = ids
                    .into_iter()
                    .map(|n| JsValue::from(make_element(n, &doc, ctx)))
                    .collect();
                let arr = JsArray::from_iter(items, ctx);
                Ok(JsValue::from(arr))
            })
        }
    };

    let get_by_class = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |_this, args, ctx| {
                // Match elements carrying ALL of the (space-separated) requested classes.
                let raw = args.first().map(|a| render_value(a, ctx)).unwrap_or_default();
                let wanted: Vec<String> =
                    raw.split_whitespace().map(|s| s.to_string()).collect();
                let mut ids = Vec::new();
                {
                    let d = doc.borrow();
                    fn walk(
                        doc: &dom::Document,
                        node: dom::NodeId,
                        wanted: &[String],
                        out: &mut Vec<dom::NodeId>,
                    ) {
                        if let dom::NodeData::Element(e) = &doc.get(node).data {
                            if !wanted.is_empty()
                                && wanted.iter().all(|w| e.classes().any(|c| c == w))
                            {
                                out.push(node);
                            }
                        }
                        for &child in &doc.get(node).children {
                            walk(doc, child, wanted, out);
                        }
                    }
                    walk(&d, d.root(), &wanted, &mut ids);
                }
                let items: Vec<JsValue> = ids
                    .into_iter()
                    .map(|n| JsValue::from(make_element(n, &doc, ctx)))
                    .collect();
                let arr = JsArray::from_iter(items, ctx);
                Ok(JsValue::from(arr))
            })
        }
    };

    // --- Native attribute accessors keyed by a node-id argument. The browser-env bootstrap
    // uses these to back live `style`/`classList`/`dataset` on element wrappers, reading and
    // writing the real DOM `attrs` so JS-driven changes survive into re-cascade. ---
    let raw_get_attr = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |_this, args, ctx| {
                let node = args.first().and_then(|v| v.as_number()).map(|n| dom::NodeId(n as usize));
                let name = args.get(1).map(|a| render_value(a, ctx)).unwrap_or_default();
                let val = node.and_then(|n| match &doc.borrow().get(n).data {
                    dom::NodeData::Element(e) => e.attrs.get(&name).cloned(),
                    _ => None,
                });
                Ok(match val {
                    Some(v) => JsValue::from(js_string!(v)),
                    None => JsValue::null(),
                })
            })
        }
    };
    let raw_set_attr = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |_this, args, ctx| {
                let node = args.first().and_then(|v| v.as_number()).map(|n| dom::NodeId(n as usize));
                let name = args.get(1).map(|a| render_value(a, ctx)).unwrap_or_default();
                let value = args.get(2).map(|a| render_value(a, ctx)).unwrap_or_default();
                if let Some(n) = node {
                    if let dom::NodeData::Element(e) = &mut doc.borrow_mut().get_mut(n).data {
                        e.attrs.insert(name, value);
                    }
                }
                Ok(JsValue::undefined())
            })
        }
    };
    let raw_remove_attr = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |_this, args, ctx| {
                let node = args.first().and_then(|v| v.as_number()).map(|n| dom::NodeId(n as usize));
                let name = args.get(1).map(|a| render_value(a, ctx)).unwrap_or_default();
                if let Some(n) = node {
                    if let dom::NodeData::Element(e) = &mut doc.borrow_mut().get_mut(n).data {
                        e.attrs.remove(&name);
                    }
                }
                Ok(JsValue::undefined())
            })
        }
    };

    let create_element = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |_this, args, ctx| {
                let tag = args.first().map(|a| render_value(a, ctx)).unwrap_or_default();
                let id = {
                    let mut d = doc.borrow_mut();
                    d.alloc(
                        dom::NodeData::Element(dom::ElementData {
                            tag,
                            attrs: std::collections::HashMap::new(),
                        }),
                        None,
                    )
                };
                Ok(JsValue::from(make_element(id, &doc, ctx)))
            })
        }
    };

    // title getter/setter
    let title_get = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |_this, _args, _ctx| {
                let d = doc.borrow();
                let s = find_by_tag(&d, d.root(), "title")
                    .map(|n| text_content(&d, n))
                    .unwrap_or_default();
                Ok(JsValue::from(js_string!(s)))
            })
        }
        .to_js_function(&realm)
    };
    let title_set = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |_this, args, ctx| {
                let text = args.first().map(|a| render_value(a, ctx)).unwrap_or_default();
                let title = {
                    let d = doc.borrow();
                    find_by_tag(&d, d.root(), "title")
                };
                if let Some(n) = title {
                    set_text_content(&mut doc.borrow_mut(), n, &text);
                }
                Ok(JsValue::undefined())
            })
        }
        .to_js_function(&realm)
    };

    let body_get = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |_this, _args, ctx| {
                let found = {
                    let d = doc.borrow();
                    find_by_tag(&d, d.root(), "body")
                };
                Ok(element_or_null(found, &doc, ctx))
            })
        }
        .to_js_function(&realm)
    };

    let doc_el_get = {
        let doc = Rc::clone(doc);
        unsafe {
            NativeFunction::from_closure(move |_this, _args, ctx| {
                let found = {
                    let d = doc.borrow();
                    find_by_tag(&d, d.root(), "html")
                };
                Ok(element_or_null(found, &doc, ctx))
            })
        }
        .to_js_function(&realm)
    };

    let attr = Attribute::all();
    let document = ObjectInitializer::new(context)
        .function(get_by_id, js_string!("getElementById"), 1)
        .function(get_by_tag, js_string!("getElementsByTagName"), 1)
        .function(get_by_class, js_string!("getElementsByClassName"), 1)
        .function(query, js_string!("querySelector"), 1)
        .function(query_all, js_string!("querySelectorAll"), 1)
        .function(create_element, js_string!("createElement"), 1)
        .function(raw_get_attr, js_string!("__getAttr"), 2)
        .function(raw_set_attr, js_string!("__setAttr"), 3)
        .function(raw_remove_attr, js_string!("__removeAttr"), 2)
        .accessor(js_string!("title"), Some(title_get), Some(title_set), attr)
        .accessor(js_string!("body"), Some(body_get), None, attr)
        .accessor(js_string!("documentElement"), Some(doc_el_get), None, attr)
        .build();

    context
        .register_global_property(js_string!("document"), document, Attribute::all())
        .expect("register document global");
}

/// Install a minimal `console` global whose `log`/`info`/`warn`/`error` push a formatted,
/// space-separated line into the shared buffer. We register our own native functions rather
/// than `boa_runtime::Console` so output is captured into our buffer instead of stdout.
fn install_console(context: &mut Context, buffer: &Rc<RefCell<Vec<String>>>) {
    let make_logger = |buffer: Rc<RefCell<Vec<String>>>| {
        // SAFETY: the closure captures only an `Rc<RefCell<Vec<String>>>`, which contains no
        // GC-traceable (`Trace`) values. Per `from_closure`'s contract, capturing only
        // non-traceable data is sound — there is nothing the GC needs to walk.
        unsafe {
            NativeFunction::from_closure(
                move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| -> JsResult<JsValue> {
                    let line = args
                        .iter()
                        .map(|a| stringify_arg(a, ctx))
                        .collect::<Vec<_>>()
                        .join(" ");
                    buffer.borrow_mut().push(line);
                    Ok(JsValue::undefined())
                },
            )
        }
    };

    let console = ObjectInitializer::new(context)
        .function(make_logger(Rc::clone(buffer)), js_string!("log"), 0)
        .function(make_logger(Rc::clone(buffer)), js_string!("info"), 0)
        .function(make_logger(Rc::clone(buffer)), js_string!("warn"), 0)
        .function(make_logger(Rc::clone(buffer)), js_string!("error"), 0)
        .function(make_logger(Rc::clone(buffer)), js_string!("debug"), 0)
        .build();

    context
        .register_global_property(js_string!("console"), console, boa_engine::property::Attribute::all())
        .expect("register console global");
}

/// JS bootstrap implementing the timer / event-loop APIs.
///
/// The whole queue (including the user-supplied callbacks) lives on a reachable JS object so
/// Boa's GC roots the callbacks for us — we deliberately keep no Boa `JsValue`/`JsObject` in
/// Rust-side state, which would break GC rooting (see the `from_closure` SAFETY notes above).
/// Rust only *drives* the loop by calling `globalThis.__runDueTimers()` and reading the
/// `__timerErrors` array; all scheduling logic is here.
///
/// Since the runtime never actually sleeps, `delay` only establishes ordering: the next due
/// timer is always the one with the smallest `when` (ties broken by id = insertion order), and
/// running it advances the virtual clock `__eventLoop.now` to its `when`.
const TIMERS_BOOTSTRAP: &str = r#"
(function () {
  var loop = { timers: [], micro: [], nextId: 1, now: 0 };
  Object.defineProperty(globalThis, "__eventLoop", { value: loop, enumerable: false, configurable: true, writable: true });
  Object.defineProperty(globalThis, "__timerErrors", { value: [], enumerable: false, configurable: true, writable: true });

  function schedule(fn, delay, args, repeat) {
    if (typeof fn !== "function") { return 0; }
    var d = Number(delay) || 0;
    if (d < 0 || d !== d) { d = 0; }
    var id = loop.nextId++;
    loop.timers.push({ id: id, fn: fn, delay: d, args: args, when: loop.now + d, repeat: repeat });
    return id;
  }

  function define(name, fn) {
    Object.defineProperty(globalThis, name, { value: fn, enumerable: false, configurable: true, writable: true });
  }

  define("setTimeout", function (fn, delay) {
    var args = Array.prototype.slice.call(arguments, 2);
    return schedule(fn, delay, args, false);
  });
  define("setInterval", function (fn, delay) {
    var args = Array.prototype.slice.call(arguments, 2);
    return schedule(fn, delay, args, true);
  });
  define("clearTimeout", function (id) {
    if (id == null) { return; }
    for (var i = 0; i < loop.timers.length; i++) {
      if (loop.timers[i].id === id) { loop.timers.splice(i, 1); return; }
    }
  });
  define("clearInterval", globalThis.clearTimeout);

  define("queueMicrotask", function (fn) {
    if (typeof fn !== "function") { throw new TypeError("queueMicrotask: argument is not a function"); }
    loop.micro.push(fn);
  });

  define("requestAnimationFrame", function (fn) {
    // No real frames; schedule ~16ms out (one 60fps frame) so rAF runs after 0ms timers.
    return schedule(fn, 16, [loop.now + 16], false);
  });
  define("cancelAnimationFrame", globalThis.clearTimeout);

  // Driver called from Rust. Returns true if it ran a task (microtask or timer), false if the
  // whole queue was empty. One throwing task does not kill the loop: errors are collected.
  define("__runDueTimers", function () {
    // 1. Drain ALL microtasks first (FIFO), including ones queued while draining.
    var ranSomething = false;
    while (loop.micro.length > 0) {
      var m = loop.micro.shift();
      ranSomething = true;
      try { m(); } catch (e) { globalThis.__timerErrors.push(String(e)); }
    }
    if (ranSomething) { return true; }

    // 2. Pick the single timer with the smallest `when`; ties broken by smallest id.
    if (loop.timers.length === 0) { return false; }
    var bestIdx = 0;
    for (var i = 1; i < loop.timers.length; i++) {
      var t = loop.timers[i], b = loop.timers[bestIdx];
      if (t.when < b.when || (t.when === b.when && t.id < b.id)) { bestIdx = i; }
    }
    var timer = loop.timers[bestIdx];
    loop.now = timer.when;
    if (timer.repeat) {
      timer.when = loop.now + timer.delay; // reschedule before running so clearInterval inside works
    } else {
      loop.timers.splice(bestIdx, 1);
    }
    try { timer.fn.apply(undefined, timer.args); }
    catch (e) { globalThis.__timerErrors.push(String(e)); }
    return true;
  });
})();
"#;

/// Install the timer / event-loop APIs (`setTimeout`, `setInterval`, `clearTimeout`,
/// `clearInterval`, `queueMicrotask`, `requestAnimationFrame`/`cancelAnimationFrame`) by
/// evaluating [`TIMERS_BOOTSTRAP`]. Must be called after `install_globals` so `globalThis`
/// aliases exist (it only touches `globalThis`, so it also works without them).
fn install_timers(context: &mut Context) {
    context
        .eval(Source::from_bytes(TIMERS_BOOTSTRAP))
        .expect("install timer bootstrap");
}

/// JS bootstrap implementing a standard "browser environment" of global APIs (`navigator`,
/// `location`, `history`, `localStorage`/`sessionStorage`, `screen`, window metrics,
/// `matchMedia`, `getComputedStyle`, a no-op event model + DOM lifecycle dispatch, and a grab
/// bag of presence stubs like `fetch`/`XMLHttpRequest`/`crypto`/`btoa`).
///
/// Everything is implemented as real reachable JS objects so feature-detection code that does
/// `Object.keys(navigator)` / `Object.assign({}, navigator)` / spreads / iterates these APIs
/// does not throw on missing globals. Callbacks registered here live on reachable JS objects,
/// so Boa's GC roots them for us — we keep no Boa values in Rust state (same discipline as
/// [`TIMERS_BOOTSTRAP`]).
///
/// `location` and the document URL fields are populated from `globalThis.__pageURL`, which Rust
/// sets via a native string property *before* this bootstrap runs (so there is no string
/// interpolation of the URL into JS source — no quoting/injection hazard).
const BROWSER_ENV_BOOTSTRAP: &str = r#"
(function () {
  function def(obj, name, value) {
    Object.defineProperty(obj, name, { value: value, enumerable: false, configurable: true, writable: true });
  }
  function fn() {}

  // --- legacy / missing language polyfills the host engine lacks ---------------------------
  // String.prototype.substr (deprecated but heavily used by real-world minified code, e.g.
  // google's URL-encoding helpers). Without it `"x".substr(1)` throws "not a callable function".
  if (typeof String.prototype.substr !== "function") {
    def(String.prototype, "substr", function (start, length) {
      var s = String(this);
      var len = s.length;
      start = start === undefined ? 0 : (start | 0);
      if (start < 0) { start = Math.max(len + start, 0); }
      var count = length === undefined ? (len - start) : (length | 0);
      if (count <= 0 || start >= len) { return ""; }
      count = Math.min(count, len - start);
      return s.slice(start, start + count);
    });
  }

  // --- navigator (plain object so enumeration / Object.keys / Object.assign work) ----------
  var ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Safari/605.1.15";
  globalThis.navigator = {
    userAgent: ua,
    appVersion: "5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Safari/605.1.15",
    appName: "Netscape",
    appCodeName: "Mozilla",
    product: "Gecko",
    platform: "MacIntel",
    vendor: "Apple Computer, Inc.",
    vendorSub: "",
    language: "en-US",
    languages: ["en-US", "en"],
    onLine: true,
    cookieEnabled: true,
    doNotTrack: null,
    maxTouchPoints: 0,
    hardwareConcurrency: 8,
    deviceMemory: 8,
    webdriver: false,
    plugins: [],
    mimeTypes: [],
    userActivation: { hasBeenActive: false, isActive: false },
    sendBeacon: function () { return false; },
    clipboard: {},
    geolocation: {
      getCurrentPosition: function () {},
      watchPosition: function () { return 0; },
      clearWatch: function () {}
    }
  };

  // --- location (populated from globalThis.__pageURL below) --------------------------------
  function parseURL(url) {
    url = String(url == null ? "" : url);
    // scheme://host/path?query#hash  (host = userinfo@hostname:port)
    var m = /^([a-zA-Z][a-zA-Z0-9+.\-]*:)?(?:\/\/([^\/?#]*))?([^?#]*)(\?[^#]*)?(#.*)?$/.exec(url) || [];
    var protocol = m[1] || "";
    var authority = m[2] || "";
    var pathname = m[3] || "";
    var search = m[4] || "";
    var hash = m[5] || "";
    // Strip any userinfo, then split host into hostname:port.
    var at = authority.lastIndexOf("@");
    var host = at >= 0 ? authority.slice(at + 1) : authority;
    var hostname = host, port = "";
    var colon = host.lastIndexOf(":");
    if (colon >= 0 && host.indexOf("]", colon) < 0) { hostname = host.slice(0, colon); port = host.slice(colon + 1); }
    if (pathname === "" && protocol && host) { pathname = "/"; }
    var origin = (protocol && host) ? (protocol + "//" + host) : "null";
    return {
      href: url, protocol: protocol, host: host, hostname: hostname, port: port,
      pathname: pathname, search: search, hash: hash, origin: origin
    };
  }

  var parts = parseURL(globalThis.__pageURL);
  var location = {
    href: parts.href, protocol: parts.protocol, host: parts.host, hostname: parts.hostname,
    port: parts.port, pathname: parts.pathname, search: parts.search, hash: parts.hash,
    origin: parts.origin,
    assign: fn, replace: fn, reload: fn,
    toString: function () { return this.href; }
  };
  // `location` already exists (a minimal stub from install_globals); overwrite it.
  globalThis.location = location;

  // --- history -----------------------------------------------------------------------------
  globalThis.history = {
    length: 1, scrollRestoration: "auto", state: null,
    pushState: fn, replaceState: fn, back: fn, forward: fn, go: fn
  };

  // --- Storage (localStorage / sessionStorage) ---------------------------------------------
  function makeStorage() {
    var map = Object.create(null);
    var s = {
      getItem: function (k) { k = String(k); return Object.prototype.hasOwnProperty.call(map, k) ? map[k] : null; },
      setItem: function (k, v) { map[String(k)] = String(v); },
      removeItem: function (k) { delete map[String(k)]; },
      clear: function () { map = Object.create(null); },
      key: function (i) { var ks = Object.keys(map); return i >= 0 && i < ks.length ? ks[i] : null; }
    };
    Object.defineProperty(s, "length", { get: function () { return Object.keys(map).length; }, enumerable: false, configurable: true });
    return s;
  }
  globalThis.localStorage = makeStorage();
  globalThis.sessionStorage = makeStorage();

  // --- screen ------------------------------------------------------------------------------
  globalThis.screen = {
    width: 1512, height: 982, availWidth: 1512, availHeight: 944,
    colorDepth: 24, pixelDepth: 24,
    orientation: { type: "landscape-primary", angle: 0 }
  };

  // --- window metrics + no-op window methods -----------------------------------------------
  globalThis.innerWidth = 1200; globalThis.innerHeight = 780;
  globalThis.outerWidth = 1200; globalThis.outerHeight = 820;
  globalThis.devicePixelRatio = 2;
  globalThis.scrollX = 0; globalThis.scrollY = 0;
  globalThis.pageXOffset = 0; globalThis.pageYOffset = 0;
  globalThis.screenX = 0; globalThis.screenY = 0; globalThis.screenLeft = 0; globalThis.screenTop = 0;
  globalThis.scrollTo = fn; globalThis.scrollBy = fn; globalThis.scroll = fn;
  globalThis.moveTo = fn; globalThis.moveBy = fn; globalThis.resizeTo = fn; globalThis.resizeBy = fn;
  globalThis.focus = fn; globalThis.blur = fn; globalThis.print = fn;
  globalThis.open = function () { return null; }; globalThis.close = fn; globalThis.stop = fn;
  globalThis.getSelection = function () { return null; };
  globalThis.alert = fn; globalThis.confirm = function () { return false; }; globalThis.prompt = function () { return null; };

  // --- matchMedia --------------------------------------------------------------------------
  globalThis.matchMedia = function (q) {
    return {
      matches: false, media: String(q), onchange: null,
      addListener: fn, removeListener: fn,
      addEventListener: fn, removeEventListener: fn,
      dispatchEvent: function () { return false; }
    };
  };

  // --- getComputedStyle --------------------------------------------------------------------
  // Proxy so any property access returns "" and getPropertyValue() returns "". Falls back to a
  // plain object with common props if Proxy is unavailable.
  globalThis.getComputedStyle = function () {
    var base = { getPropertyValue: function () { return ""; }, getPropertyPriority: function () { return ""; }, setProperty: fn, removeProperty: function () { return ""; }, item: function () { return ""; }, length: 0 };
    try {
      return new Proxy(base, {
        get: function (target, prop) {
          if (prop in target) { return target[prop]; }
          return "";
        }
      });
    } catch (e) {
      var common = ["display", "color", "width", "height", "visibility", "opacity", "position", "margin", "padding", "font-size", "background-color"];
      for (var i = 0; i < common.length; i++) { base[common[i]] = ""; }
      return base;
    }
  };

  // --- event model (no-op but present) + a simple listener registry ------------------------
  function installEvents(target) {
    if (!target || typeof target !== "object") { return; }
    if (target.__listeners) { return; } // already installed
    var registry = Object.create(null);
    def(target, "__listeners", registry);
    def(target, "addEventListener", function (type, cb) {
      if (typeof cb !== "function") { return; }
      type = String(type);
      (registry[type] || (registry[type] = [])).push(cb);
    });
    def(target, "removeEventListener", function (type, cb) {
      type = String(type);
      var list = registry[type];
      if (!list) { return; }
      for (var i = 0; i < list.length; i++) { if (list[i] === cb) { list.splice(i, 1); return; } }
    });
    def(target, "dispatchEvent", function (ev) {
      var type = ev && ev.type ? String(ev.type) : "";
      var list = registry[type];
      if (list) {
        var copy = list.slice();
        for (var i = 0; i < copy.length; i++) {
          try { copy[i].call(target, ev); } catch (e) { (globalThis.__timerErrors || []).push(String(e)); }
        }
      }
      return true;
    });
  }
  installEvents(globalThis);
  installEvents(document);

  // --- DOM lifecycle dispatch (driven from Rust during the drain) --------------------------
  var readyState = "loading";
  Object.defineProperty(document, "readyState", { get: function () { return readyState; }, enumerable: true, configurable: true });
  document.referrer = "";
  document.URL = parts.href;
  document.documentURI = parts.href;
  document.baseURI = parts.href;
  document.domain = parts.hostname;
  document.title = document.title; // leave as-is; real getter/setter already present

  // `document.currentScript`: real browsers return the executing <script> element. We don't
  // track it, so expose a harmless stub element (with a no-op remove()) so inline bootstraps
  // like `document.currentScript.remove()` (TanStack/React hydration) don't throw.
  document.currentScript = {
    remove: fn, setAttribute: fn, getAttribute: function () { return null; },
    removeAttribute: fn, hasAttribute: function () { return false; },
    addEventListener: fn, removeEventListener: fn, appendChild: function (c) { return c; },
    parentNode: null, parentElement: null, nextSibling: null, previousSibling: null,
    src: "", type: "", async: false, defer: false, dataset: {}, style: {},
  };

  function makeEvent(type) {
    return { type: type, target: document, currentTarget: document, bubbles: false, cancelable: false,
             defaultPrevented: false, timeStamp: 0,
             preventDefault: fn, stopPropagation: fn, stopImmediatePropagation: fn };
  }
  function fireOn(target, type) {
    if (target && typeof target.dispatchEvent === "function") {
      try { target.dispatchEvent(makeEvent(type)); } catch (e) { (globalThis.__timerErrors || []).push(String(e)); }
    }
    // Also invoke an `on<type>` handler if one was assigned (e.g. window.onload = ...).
    var on = target ? target["on" + type] : null;
    if (typeof on === "function") {
      try { on.call(target, makeEvent(type)); } catch (e) { (globalThis.__timerErrors || []).push(String(e)); }
    }
  }
  // Called from Rust's drain phase, in order, to advance readyState and fire lifecycle events.
  def(globalThis, "__fireLifecycleEvents", function () {
    readyState = "interactive";
    fireOn(document, "readystatechange");
    fireOn(document, "DOMContentLoaded");
    readyState = "complete";
    fireOn(document, "readystatechange");
    fireOn(window, "load");
    fireOn(document, "load");
    fireOn(window, "pageshow");
  });

  // --- document extras ---------------------------------------------------------------------
  var cookieStore = "";
  Object.defineProperty(document, "cookie", {
    get: function () { return cookieStore; },
    set: function (v) {
      v = String(v);
      var pair = v.split(";")[0];
      if (pair.indexOf("=") >= 0) { cookieStore = cookieStore ? (cookieStore + "; " + pair) : pair; }
    },
    enumerable: true, configurable: true
  });

  // head / documentElement may be missing from the native document; add lazy getters that
  // resolve via querySelector without clobbering existing accessors (e.g. `body`).
  function ensureGetter(name, selector) {
    var d = Object.getOwnPropertyDescriptor(document, name);
    if (d && (d.get || d.value)) { return; }
    Object.defineProperty(document, name, {
      get: function () { try { return document.querySelector(selector); } catch (e) { return null; } },
      enumerable: true, configurable: true
    });
  }
  ensureGetter("head", "head");
  // documentElement / body already exist as native accessors; only add head defensively.

  // getElementsByClassName is now a real native binding on document; nothing to add here.

  // --- write-through style / classList / dataset, backed by the real DOM attrs --------------
  // All three read and write the element's `style` / `class` / `data-*` attributes in the shared
  // document via the native `document.__getAttr/__setAttr/__removeAttr(node, name[, value])`
  // helpers, keyed by the wrapper's hidden `__node` id. This is what makes JS-driven style/class
  // changes survive into the engine's re-cascade and actually re-render.

  // Parse `prop: value; ...` into an ordered list of [prop, value] pairs (lowercased props).
  function parseStyleDecls(text) {
    var out = [];
    text = String(text || "");
    var parts = text.split(";");
    for (var i = 0; i < parts.length; i++) {
      var seg = parts[i];
      var c = seg.indexOf(":");
      if (c < 0) { continue; }
      var name = seg.slice(0, c).trim().toLowerCase();
      var val = seg.slice(c + 1).trim();
      if (name) { out.push([name, val]); }
    }
    return out;
  }
  function serializeStyleDecls(decls) {
    var s = "";
    for (var i = 0; i < decls.length; i++) { s += (s ? " " : "") + decls[i][0] + ": " + decls[i][1] + ";"; }
    return s;
  }
  // camelCase JS property -> kebab-case CSS property (e.g. backgroundColor -> background-color).
  function camelToKebab(p) {
    p = String(p);
    if (p.indexOf("-") >= 0) { return p.toLowerCase(); } // already kebab (e.g. via setProperty)
    // Leading vendor prefix like `webkitTransform` -> `-webkit-transform`.
    var out = p.replace(/([A-Z])/g, function (m) { return "-" + m.toLowerCase(); });
    if (/^(webkit|moz|ms|o)-/.test(out)) { out = "-" + out; }
    return out;
  }
  function kebabToCamel(p) {
    p = String(p);
    return p.replace(/-([a-z])/g, function (_, c) { return c.toUpperCase(); });
  }
  function styleAttr(node) { var v = document.__getAttr(node, "style"); return v == null ? "" : v; }
  function makeStyle(node) {
    function read() { return parseStyleDecls(styleAttr(node)); }
    function find(decls, name) { for (var i = 0; i < decls.length; i++) { if (decls[i][0] === name) { return i; } } return -1; }
    function getVal(name) { var d = read(); var i = find(d, name); return i >= 0 ? d[i][1] : ""; }
    function setVal(name, val) {
      var d = read(); var i = find(d, name);
      if (val == null || val === "") {
        if (i >= 0) { d.splice(i, 1); }
      } else {
        val = String(val);
        if (i >= 0) { d[i][1] = val; } else { d.push([name, val]); }
      }
      document.__setAttr(node, "style", serializeStyleDecls(d));
    }
    var base = {
      getPropertyValue: function (p) { return getVal(String(p).toLowerCase()); },
      getPropertyPriority: function () { return ""; },
      setProperty: function (p, v) { setVal(String(p).toLowerCase(), v); },
      removeProperty: function (p) { p = String(p).toLowerCase(); var old = getVal(p); setVal(p, ""); return old; },
      item: function (i) { var d = read(); return i >= 0 && i < d.length ? d[i][0] : ""; }
    };
    Object.defineProperty(base, "length", { get: function () { return read().length; }, enumerable: false, configurable: true });
    Object.defineProperty(base, "cssText", {
      get: function () { return styleAttr(node); },
      set: function (v) { document.__setAttr(node, "style", serializeStyleDecls(parseStyleDecls(v))); },
      enumerable: true, configurable: true
    });
    try {
      return new Proxy(base, {
        get: function (t, p) {
          if (typeof p !== "string") { return t[p]; }
          if (p in t) { return t[p]; }
          return getVal(camelToKebab(p));
        },
        set: function (t, p, v) {
          if (typeof p !== "string") { t[p] = v; return true; }
          if (p === "cssText") { t.cssText = v; return true; }
          setVal(camelToKebab(p), v); return true;
        }
      });
    } catch (e) { return base; }
  }
  function makeClassList(node) {
    function read() { var c = document.__getAttr(node, "class"); return c ? String(c).split(/\s+/).filter(Boolean) : []; }
    function write(arr) {
      var seen = Object.create(null), out = [];
      for (var i = 0; i < arr.length; i++) { if (!seen[arr[i]]) { seen[arr[i]] = 1; out.push(arr[i]); } }
      document.__setAttr(node, "class", out.join(" "));
    }
    var cl = {
      add: function () { var c = read(); for (var i = 0; i < arguments.length; i++) { var n = String(arguments[i]); if (c.indexOf(n) < 0) c.push(n); } write(c); },
      remove: function () { var c = read(); for (var i = 0; i < arguments.length; i++) { var x = c.indexOf(String(arguments[i])); if (x >= 0) c.splice(x, 1); } write(c); },
      toggle: function (n, force) {
        n = String(n); var c = read(); var x = c.indexOf(n);
        if (force === true) { if (x < 0) { c.push(n); write(c); } return true; }
        if (force === false) { if (x >= 0) { c.splice(x, 1); write(c); } return false; }
        if (x >= 0) { c.splice(x, 1); write(c); return false; } c.push(n); write(c); return true;
      },
      replace: function (oldC, newC) { var c = read(); var x = c.indexOf(String(oldC)); if (x >= 0) { c[x] = String(newC); write(c); return true; } return false; },
      contains: function (n) { return read().indexOf(String(n)) >= 0; },
      item: function (i) { var c = read(); return i >= 0 && i < c.length ? c[i] : null; },
      forEach: function (cb, thisArg) { var c = read(); for (var i = 0; i < c.length; i++) { cb.call(thisArg, c[i], i, this); } },
      toString: function () { return read().join(" "); }
    };
    Object.defineProperty(cl, "length", { get: function () { return read().length; }, enumerable: false, configurable: true });
    Object.defineProperty(cl, "value", {
      get: function () { return read().join(" "); },
      set: function (v) { document.__setAttr(node, "class", String(v)); },
      enumerable: false, configurable: true
    });
    return cl;
  }
  function makeDataset(node) {
    // Live view over data-* attributes. dataset.fooBar <-> data-foo-bar.
    var base = {};
    try {
      return new Proxy(base, {
        get: function (t, p) {
          if (typeof p !== "string") { return t[p]; }
          var v = document.__getAttr(node, "data-" + camelToKebab(p));
          return v == null ? undefined : v;
        },
        set: function (t, p, v) { if (typeof p === "string") { document.__setAttr(node, "data-" + camelToKebab(p), v == null ? "" : String(v)); } return true; },
        deleteProperty: function (t, p) { if (typeof p === "string") { document.__removeAttr(node, "data-" + camelToKebab(p)); } return true; },
        has: function (t, p) { return typeof p === "string" && document.__getAttr(node, "data-" + camelToKebab(p)) != null; }
      });
    } catch (e) { return base; }
  }
  function makeRect() { return { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0, toJSON: function () { return this; } }; }

  // --- per-node wrapper cache (stable identity + expando persistence) ----------------------
  // Native DOM methods/accessors return a FRESH wrapper object on every call (each carrying the
  // hidden `__node` id). Frameworks like Vue stash internal state directly on DOM nodes
  // (`el.__vnode`, `el._vei`, `el.$once`, ...) and rely on `getElementById(x) === getElementById(x)`
  // and on those expandos surviving across lookups. To honor that we keep a JS-side map from node
  // id -> the one canonical enriched wrapper, and route every element the native layer hands back
  // through `canon()`, which returns the cached wrapper (copying over the fresh wrapper's own
  // function bindings on first sight). The cache lives entirely on the JS side, so Boa's GC roots
  // the wrappers for us — no Boa values are held in Rust (same discipline as elsewhere).
  var __nodeCache = Object.create(null);
  function canon(el) {
    if (!el || typeof el !== "object") { return el; }
    var node = el.__node;
    if (typeof node !== "number") { return enrichElement(el); }
    var cached = __nodeCache[node];
    if (cached) { return cached; }
    __nodeCache[node] = el;       // record BEFORE enriching so re-entrant lookups dedupe
    enrichElement(el);
    return el;
  }
  def(globalThis, "__canonNode", canon);

  // Map a tag name to the most specific DOM interface prototype we have, so element wrappers
  // satisfy `el instanceof HTMLElement/Element/Node` (and SVG/MathML where appropriate). The
  // wrapper keeps all its own (native) accessors/methods; we only graft the interface prototype
  // onto its chain via Object.setPrototypeOf, then re-install its own data/accessor props (they
  // are own properties on the wrapper, so the chain swap doesn't lose them).
  var svgTags = { svg: 1, path: 1, g: 1, rect: 1, circle: 1, ellipse: 1, line: 1, polyline: 1,
    polygon: 1, text: 1, tspan: 1, defs: 1, use: 1, symbol: 1, marker: 1, "clippath": 1,
    mask: 1, pattern: 1, image: 1, "lineargradient": 1, "radialgradient": 1, stop: 1, filter: 1,
    foreignobject: 1 };
  var tagIface = {
    div: "HTMLDivElement", span: "HTMLSpanElement", p: "HTMLParagraphElement", a: "HTMLAnchorElement",
    img: "HTMLImageElement", input: "HTMLInputElement", button: "HTMLButtonElement",
    select: "HTMLSelectElement", option: "HTMLOptionElement", textarea: "HTMLTextAreaElement",
    form: "HTMLFormElement", label: "HTMLLabelElement", ul: "HTMLUListElement", ol: "HTMLOListElement",
    li: "HTMLLIElement", table: "HTMLTableElement", tr: "HTMLTableRowElement", td: "HTMLTableCellElement",
    th: "HTMLTableCellElement", canvas: "HTMLCanvasElement", video: "HTMLVideoElement",
    audio: "HTMLAudioElement", iframe: "HTMLIFrameElement", template: "HTMLTemplateElement",
    h1: "HTMLHeadingElement", h2: "HTMLHeadingElement", h3: "HTMLHeadingElement",
    h4: "HTMLHeadingElement", h5: "HTMLHeadingElement", h6: "HTMLHeadingElement",
    body: "HTMLBodyElement", html: "HTMLHtmlElement", head: "HTMLHeadElement",
    script: "HTMLScriptElement", style: "HTMLStyleElement", link: "HTMLLinkElement",
    meta: "HTMLMetaElement", title: "HTMLTitleElement"
  };
  function ifaceProtoForTag(tag) {
    tag = String(tag || "").toLowerCase();
    if (svgTags[tag]) { return (globalThis.SVGElement && globalThis.SVGElement.prototype) || null; }
    var name = tagIface[tag];
    var ctor = name && globalThis[name];
    if (typeof ctor === "function" && ctor.prototype) { return ctor.prototype; }
    return (globalThis.HTMLElement && globalThis.HTMLElement.prototype) || null;
  }

  function enrichElement(el) {
    if (!el || typeof el !== "object") { return el; }
    if (el.__enriched) { return el; }
    var node = el.__node;
    def(el, "__enriched", true);
    // Graft the matching DOM interface prototype onto the wrapper's chain (own props survive).
    if (typeof node === "number") {
      try {
        var tag = el.tagName;
        var proto = ifaceProtoForTag(tag);
        if (proto && Object.getPrototypeOf(el) !== proto) { Object.setPrototypeOf(el, proto); }
      } catch (e) {}
    }
    if (typeof node === "number") {
      def(el, "style", makeStyle(node));
      def(el, "classList", makeClassList(node));
      def(el, "dataset", makeDataset(node));
    } else {
      // Detached/foreign object: fall back to inert stubs so access doesn't throw.
      if (!("style" in el) || el.style == null) { def(el, "style", { getPropertyValue: function () { return ""; }, setProperty: fn, removeProperty: function () { return ""; }, cssText: "" }); }
      if (!("classList" in el) || el.classList == null) { def(el, "classList", { add: fn, remove: fn, toggle: function () { return false; }, contains: function () { return false; }, item: function () { return null; } }); }
      if (!("dataset" in el) || el.dataset == null) { def(el, "dataset", {}); }
    }
    // Element-returning native methods hand back un-enriched wrappers; wrap them so the result
    // is enriched (gets style/classList/dataset) before page code touches it.
    var elemMethods = ["querySelector", "closest"];
    for (var mi = 0; mi < elemMethods.length; mi++) {
      (function (mn) {
        var orig = el[mn];
        if (typeof orig === "function") { def(el, mn, function () { return canon(orig.apply(this, arguments)); }); }
      })(elemMethods[mi]);
    }
    var listMethods = ["querySelectorAll", "getElementsByTagName", "getElementsByClassName"];
    for (var li = 0; li < listMethods.length; li++) {
      (function (mn) {
        var orig = el[mn];
        if (typeof orig === "function") { def(el, mn, function () { var r = orig.apply(this, arguments); if (r && typeof r.length === "number") { for (var i = 0; i < r.length; i++) { r[i] = canon(r[i]); } } return r; }); }
      })(listMethods[mi]);
    }
    // Navigation accessors return fresh wrappers each time; re-wrap to canonicalize on read.
    var navAccessors = ["parentNode", "parentElement", "firstChild", "lastChild", "firstElementChild",
                        "nextSibling", "previousSibling", "nextElementSibling", "previousElementSibling"];
    for (var ni = 0; ni < navAccessors.length; ni++) {
      (function (an) {
        var d = Object.getOwnPropertyDescriptor(el, an);
        if (d && d.get) { var og = d.get; Object.defineProperty(el, an, { get: function () { return canon(og.call(this)); }, configurable: true, enumerable: d.enumerable }); }
      })(navAccessors[ni]);
    }
    var listAccessors = ["children", "childNodes"];
    for (var ci = 0; ci < listAccessors.length; ci++) {
      (function (an) {
        var d = Object.getOwnPropertyDescriptor(el, an);
        if (d && d.get) { var og = d.get; Object.defineProperty(el, an, { get: function () { var r = og.call(this); if (r && typeof r.length === "number") { for (var i = 0; i < r.length; i++) { r[i] = canon(r[i]); } } return r; }, configurable: true, enumerable: d.enumerable }); }
      })(listAccessors[ci]);
    }

    if (typeof el.getBoundingClientRect !== "function") { def(el, "getBoundingClientRect", makeRect); }
    if (typeof el.getClientRects !== "function") { def(el, "getClientRects", function () { return []; }); }
    if (typeof el.scrollIntoView !== "function") { def(el, "scrollIntoView", fn); }
    if (typeof el.focus !== "function") { def(el, "focus", fn); }
    if (typeof el.blur !== "function") { def(el, "blur", fn); }
    if (typeof el.click !== "function") { def(el, "click", fn); }
    if (typeof el.cloneNode !== "function") { def(el, "cloneNode", function () { return this; }); }
    if (typeof el.hasChildNodes !== "function") { def(el, "hasChildNodes", function () { try { return (this.childNodes || []).length > 0; } catch (e) { return false; } }); }
    if (!("nodeType" in el)) { def(el, "nodeType", 1); }
    if (!("ownerDocument" in el)) { def(el, "ownerDocument", document); }
    if (!("scrollTop" in el)) { el.scrollTop = 0; }
    if (!("scrollLeft" in el)) { el.scrollLeft = 0; }
    if (!("offsetWidth" in el)) { el.offsetWidth = 0; }
    if (!("offsetHeight" in el)) { el.offsetHeight = 0; }
    if (!("clientWidth" in el)) { el.clientWidth = 0; }
    if (!("clientHeight" in el)) { el.clientHeight = 0; }
    installEvents(el);
    return el;
  }
  // Expose so element-returning native accessors (parentNode, etc.) can be enriched lazily by
  // anything that needs it. (Kept non-enumerable.)
  def(globalThis, "__enrichElement", enrichElement);

  function wrapReturningElement(obj, name) {
    var orig = obj[name];
    if (typeof orig !== "function") { return; }
    def(obj, name, function () {
      var r = orig.apply(this, arguments);
      if (r && typeof r === "object") {
        if (typeof r.length === "number" && typeof r.splice === "function") {
          for (var i = 0; i < r.length; i++) { r[i] = canon(r[i]); }
        } else {
          return canon(r);
        }
      }
      return r;
    });
  }
  wrapReturningElement(document, "createElement");
  wrapReturningElement(document, "getElementById");
  wrapReturningElement(document, "getElementsByTagName");
  wrapReturningElement(document, "getElementsByClassName");
  wrapReturningElement(document, "querySelector");
  wrapReturningElement(document, "querySelectorAll");

  // Enrich element wrappers returned by the native element-navigation accessors and methods.
  // These return fresh wrapper objects each time, so wrap the prototype-less accessors by
  // intercepting via getter wrappers is impractical; instead wrap the element-returning methods
  // on a per-element basis when an element is first enriched. We patch the document-level
  // accessors (body/documentElement/head) below.
  function enrichDocAccessor(name) {
    var d = Object.getOwnPropertyDescriptor(document, name);
    if (!d || !d.get) { return; }
    var origGet = d.get;
    Object.defineProperty(document, name, {
      get: function () { return canon(origGet.call(this)); },
      enumerable: d.enumerable, configurable: true
    });
  }
  enrichDocAccessor("body");
  enrichDocAccessor("documentElement");
  enrichDocAccessor("head");

  // --- document node-creation helpers ------------------------------------------------------
  // createTextNode / createComment / createDocumentFragment return lightweight node-ish objects.
  // They aren't backed by the real DOM arena (only createElement is), but they are appendable to
  // real elements as no-ops and carry the properties scripts read, so init code doesn't throw.
  if (typeof document.createTextNode !== "function") {
    def(document, "createTextNode", function (data) {
      return { nodeType: 3, nodeName: "" + String.fromCharCode(35) + "text", data: String(data == null ? "" : data),
               textContent: String(data == null ? "" : data), nodeValue: String(data == null ? "" : data),
               parentNode: null, childNodes: [], appendChild: function (c) { return c; }, cloneNode: function () { return this; } };
    });
  }
  if (typeof document.createComment !== "function") {
    def(document, "createComment", function (data) {
      return { nodeType: 8, nodeName: "" + String.fromCharCode(35) + "comment", data: String(data == null ? "" : data),
               textContent: String(data == null ? "" : data), nodeValue: String(data == null ? "" : data),
               parentNode: null, childNodes: [], cloneNode: function () { return this; } };
    });
  }
  if (typeof document.createDocumentFragment !== "function") {
    def(document, "createDocumentFragment", function () {
      var kids = [];
      return { nodeType: 11, nodeName: "" + String.fromCharCode(35) + "document-fragment", childNodes: kids,
               appendChild: function (c) { kids.push(c); return c; },
               querySelector: function () { return null; }, querySelectorAll: function () { return []; },
               cloneNode: function () { return this; }, get firstChild() { return kids[0] || null; },
               get lastChild() { return kids[kids.length - 1] || null; }, get children() { return kids; } };
    });
  }
  if (typeof document.getElementsByName !== "function") {
    def(document, "getElementsByName", function (n) { try { return document.querySelectorAll('[name="' + String(n) + '"]'); } catch (e) { return []; } });
  }
  if (typeof document.contains !== "function") {
    def(document, "contains", function (node) { try { return document.documentElement ? (document.documentElement === node || document.documentElement.contains(node)) : false; } catch (e) { return false; } });
  }
  if (typeof document.createEvent !== "function") {
    def(document, "createEvent", function () { var e = { type: "", bubbles: false, cancelable: false, defaultPrevented: false, preventDefault: fn, stopPropagation: fn, initEvent: function (t, b, c) { this.type = String(t); this.bubbles = !!b; this.cancelable = !!c; }, initCustomEvent: function (t, b, c, d) { this.type = String(t); this.bubbles = !!b; this.cancelable = !!c; this.detail = d; } }; return e; });
  }
  if (typeof document.elementFromPoint !== "function") { def(document, "elementFromPoint", function () { return null; }); }
  if (typeof document.hasFocus !== "function") { def(document, "hasFocus", function () { return true; }); }
  if (!("activeElement" in document)) { Object.defineProperty(document, "activeElement", { get: function () { try { return document.body; } catch (e) { return null; } }, enumerable: true, configurable: true }); }
  if (!("visibilityState" in document)) { document.visibilityState = "visible"; }
  if (!("hidden" in document)) { document.hidden = false; }
  if (!("characterSet" in document)) { document.characterSet = "UTF-8"; }
  if (!("compatMode" in document)) { document.compatMode = "CSS1Compat"; }
  if (!("scrollingElement" in document)) { Object.defineProperty(document, "scrollingElement", { get: function () { try { return document.documentElement; } catch (e) { return null; } }, enumerable: true, configurable: true }); }
  if (typeof document.querySelectorAll === "function" && typeof document.querySelectorAll.call === "function") { /* present */ }

  // --- document.fonts (FontFaceSet) --------------------------------------------------------
  if (!("fonts" in document) || document.fonts == null) {
    var fontFaces = {
      status: "loaded", size: 0,
      ready: Promise.resolve(),
      load: function () { return Promise.resolve([]); },
      check: function () { return true; },
      add: fn, delete: function () { return false; }, has: function () { return false; },
      clear: fn, forEach: fn,
      addEventListener: fn, removeEventListener: fn, dispatchEvent: function () { return false; },
      onloading: null, onloadingdone: null, onloadingerror: null
    };
    // `ready` should be a thenable that resolves to the set itself (per spec resolves to the
    // FontFaceSet). Make it resolve to the set without creating a cycle in JSON paths.
    fontFaces.ready = Promise.resolve(fontFaces);
    Object.defineProperty(document, "fonts", { value: fontFaces, enumerable: false, configurable: true, writable: true });
  }

  // --- Observer constructors (presence + no-op observe/disconnect/takeRecords) --------------
  function makeObserver(name) {
    def(globalThis, name, function (cb) {
      this.callback = typeof cb === "function" ? cb : fn;
      this.observe = fn; this.unobserve = fn; this.disconnect = fn;
      this.takeRecords = function () { return []; };
    });
  }
  if (typeof globalThis.MutationObserver !== "function") { makeObserver("MutationObserver"); }
  if (typeof globalThis.IntersectionObserver !== "function") {
    def(globalThis, "IntersectionObserver", function (cb, opts) {
      this.callback = typeof cb === "function" ? cb : fn;
      this.root = (opts && opts.root) || null; this.rootMargin = (opts && opts.rootMargin) || "0px";
      this.thresholds = (opts && [].concat(opts.threshold || 0)) || [0];
      this.observe = fn; this.unobserve = fn; this.disconnect = fn; this.takeRecords = function () { return []; };
    });
  }
  if (typeof globalThis.ResizeObserver !== "function") { makeObserver("ResizeObserver"); }
  if (typeof globalThis.PerformanceObserver !== "function") {
    def(globalThis, "PerformanceObserver", function (cb) {
      this.callback = typeof cb === "function" ? cb : fn;
      this.observe = fn; this.disconnect = fn; this.takeRecords = function () { return []; };
    });
    globalThis.PerformanceObserver.supportedEntryTypes = [];
  }

  // --- performance -------------------------------------------------------------------------
  if (!globalThis.performance || typeof globalThis.performance.now !== "function") {
    var perfStart = 0;
    globalThis.performance = {
      now: function () { try { return (globalThis.__eventLoop ? globalThis.__eventLoop.now : 0); } catch (e) { return 0; } },
      timeOrigin: 0,
      timing: { navigationStart: 0, fetchStart: 0, domLoading: 0, domInteractive: 0, domContentLoadedEventStart: 0, domContentLoadedEventEnd: 0, domComplete: 0, loadEventStart: 0, loadEventEnd: 0, responseStart: 0, responseEnd: 0, requestStart: 0, connectStart: 0, connectEnd: 0, secureConnectionStart: 0, domainLookupStart: 0, domainLookupEnd: 0, unloadEventStart: 0, unloadEventEnd: 0, redirectStart: 0, redirectEnd: 0 },
      navigation: { type: 0, redirectCount: 0 },
      memory: { usedJSHeapSize: 0, totalJSHeapSize: 0, jsHeapSizeLimit: 0 },
      getEntries: function () { return []; },
      getEntriesByType: function () { return []; },
      getEntriesByName: function () { return []; },
      mark: fn, measure: fn, clearMarks: fn, clearMeasures: fn, clearResourceTimings: fn,
      setResourceTimingBufferSize: fn,
      toJSON: function () { return {}; }
    };
  }

  // --- IdleDeadline-style object is already provided via requestIdleCallback above. ---------

  // --- a few more constructors pages feature-detect ----------------------------------------
  if (typeof globalThis.DOMParser !== "function") {
    def(globalThis, "DOMParser", function () { this.parseFromString = function () { return document; }; });
  }
  if (typeof globalThis.IntersectionObserverEntry !== "function") { def(globalThis, "IntersectionObserverEntry", function () {}); }
  if (typeof globalThis.MutationRecord !== "function") { def(globalThis, "MutationRecord", function () {}); }
  // --- DOM interface constructors / class hierarchy ----------------------------------------
  // Vue (and most frameworks) do `el instanceof SVGElement`, read `Node.prototype`, check
  // `typeof HTMLElement === "function"`, and reference HTMLUnknownElement/Text/Comment/etc.
  // We define each as a real constructor function carrying a `.prototype` and wire up a
  // prototype chain (HTMLDivElement -> HTMLElement -> Element -> Node) so prototype walks and
  // `instanceof` checks behave. The element wrappers' prototype is set to HTMLElement.prototype
  // (see __wrapNode below) so `el instanceof HTMLElement/Element/Node` returns true.
  function defClass(name, parentCtor) {
    if (typeof globalThis[name] === "function") { return globalThis[name]; }
    var ctor = function () {};
    if (parentCtor && parentCtor.prototype) {
      try { Object.setPrototypeOf(ctor.prototype, parentCtor.prototype); } catch (e) {}
    }
    def(globalThis, name, ctor);
    return ctor;
  }
  var NodeCtor = defClass("Node");
  NodeCtor.ELEMENT_NODE = 1; NodeCtor.ATTRIBUTE_NODE = 2; NodeCtor.TEXT_NODE = 3;
  NodeCtor.CDATA_SECTION_NODE = 4; NodeCtor.PROCESSING_INSTRUCTION_NODE = 7; NodeCtor.COMMENT_NODE = 8;
  NodeCtor.DOCUMENT_NODE = 9; NodeCtor.DOCUMENT_TYPE_NODE = 10; NodeCtor.DOCUMENT_FRAGMENT_NODE = 11;
  defClass("EventTarget");
  defClass("CharacterData", NodeCtor);
  defClass("Text", globalThis.CharacterData);
  defClass("Comment", globalThis.CharacterData);
  defClass("CDATASection", globalThis.Text);
  defClass("ProcessingInstruction", globalThis.CharacterData);
  defClass("DocumentFragment", NodeCtor);
  defClass("ShadowRoot", globalThis.DocumentFragment);
  defClass("DocumentType", NodeCtor);
  defClass("Attr", NodeCtor);
  var ElementCtor = defClass("Element", NodeCtor);
  var HTMLElementCtor = defClass("HTMLElement", ElementCtor);
  defClass("SVGElement", ElementCtor);
  defClass("SVGSVGElement", globalThis.SVGElement);
  defClass("SVGGraphicsElement", globalThis.SVGElement);
  defClass("MathMLElement", ElementCtor);
  defClass("HTMLUnknownElement", HTMLElementCtor);
  // A broad set of concrete HTMLElement subclasses pages feature-detect / reference.
  var htmlSubclasses = [
    "HTMLDivElement", "HTMLSpanElement", "HTMLParagraphElement", "HTMLAnchorElement",
    "HTMLImageElement", "HTMLInputElement", "HTMLButtonElement", "HTMLSelectElement",
    "HTMLOptionElement", "HTMLOptGroupElement", "HTMLTextAreaElement", "HTMLFormElement",
    "HTMLLabelElement", "HTMLUListElement", "HTMLOListElement", "HTMLLIElement",
    "HTMLTableElement", "HTMLTableRowElement", "HTMLTableCellElement", "HTMLTableSectionElement",
    "HTMLTableColElement", "HTMLTableCaptionElement", "HTMLHeadingElement", "HTMLPreElement",
    "HTMLQuoteElement", "HTMLHRElement", "HTMLBRElement", "HTMLScriptElement",
    "HTMLStyleElement", "HTMLLinkElement", "HTMLMetaElement", "HTMLTitleElement",
    "HTMLHeadElement", "HTMLBodyElement", "HTMLHtmlElement", "HTMLCanvasElement",
    "HTMLVideoElement", "HTMLAudioElement", "HTMLMediaElement", "HTMLSourceElement",
    "HTMLTrackElement", "HTMLIFrameElement", "HTMLEmbedElement", "HTMLObjectElement",
    "HTMLPictureElement", "HTMLTemplateElement", "HTMLSlotElement", "HTMLDataListElement",
    "HTMLFieldSetElement", "HTMLLegendElement", "HTMLDetailsElement", "HTMLDialogElement",
    "HTMLMenuElement", "HTMLMapElement", "HTMLAreaElement", "HTMLDListElement",
    "HTMLDataElement", "HTMLTimeElement", "HTMLOutputElement", "HTMLProgressElement",
    "HTMLMeterElement", "HTMLModElement", "HTMLFontElement", "HTMLDirectoryElement",
    "HTMLMarqueeElement"
  ];
  // HTMLMediaElement should sit under HTMLElement; audio/video under it. Keep flat-under-HTMLElement
  // for simplicity except a couple that pages explicitly chain.
  for (var hi = 0; hi < htmlSubclasses.length; hi++) { defClass(htmlSubclasses[hi], HTMLElementCtor); }

  // --- Image / Audio / media element constructors ------------------------------------------
  if (typeof globalThis.Image !== "function") {
    def(globalThis, "Image", function (w, h) {
      this.width = w || 0; this.height = h || 0; this.naturalWidth = 0; this.naturalHeight = 0;
      this.complete = false; this.src = ""; this.alt = ""; this.crossOrigin = null; this.decoding = "auto";
      this.onload = null; this.onerror = null;
      this.setAttribute = fn; this.getAttribute = function () { return null; };
      this.addEventListener = fn; this.removeEventListener = fn; this.dispatchEvent = function () { return false; };
      this.decode = function () { return Promise.resolve(); };
      try { def(this, "style", { setProperty: fn, getPropertyValue: function () { return ""; }, removeProperty: function () { return ""; }, cssText: "" }); } catch (e) {}
    });
    def(globalThis, "HTMLImageElement", globalThis.Image);
  }
  if (typeof globalThis.Audio !== "function") {
    def(globalThis, "Audio", function (src) {
      this.src = src || ""; this.currentTime = 0; this.paused = true; this.volume = 1;
      this.play = function () { return Promise.resolve(); }; this.pause = fn; this.load = fn;
      this.canPlayType = function () { return ""; };
      this.addEventListener = fn; this.removeEventListener = fn;
    });
  }
  // --- Blob / File / FileReader / Worker presence stubs ------------------------------------
  if (typeof globalThis.Blob !== "function") {
    def(globalThis, "Blob", function (parts, opts) {
      this.size = 0; this.type = (opts && opts.type) || "";
      this.slice = function () { return new globalThis.Blob([], { type: this.type }); };
      this.text = function () { return Promise.resolve(""); };
      this.arrayBuffer = function () { return Promise.resolve(new ArrayBuffer(0)); };
    });
  }
  if (typeof globalThis.File !== "function") {
    def(globalThis, "File", function (parts, name, opts) { globalThis.Blob.call(this, parts, opts); this.name = String(name || ""); this.lastModified = 0; });
  }
  if (typeof globalThis.FileReader !== "function") {
    def(globalThis, "FileReader", function () {
      this.readyState = 0; this.result = null; this.error = null;
      this.onload = null; this.onloadend = null; this.onerror = null;
      this.readAsText = fn; this.readAsDataURL = fn; this.readAsArrayBuffer = fn; this.abort = fn;
      this.addEventListener = fn; this.removeEventListener = fn;
    });
  }
  if (typeof globalThis.Worker !== "function") {
    def(globalThis, "Worker", function () { this.postMessage = fn; this.terminate = fn; this.onmessage = null; this.onerror = null; this.addEventListener = fn; this.removeEventListener = fn; });
  }
  if (typeof globalThis.WebSocket !== "function") {
    def(globalThis, "WebSocket", function () { this.readyState = 3; this.send = fn; this.close = fn; this.onopen = null; this.onmessage = null; this.onerror = null; this.onclose = null; this.addEventListener = fn; this.removeEventListener = fn; });
    globalThis.WebSocket.CONNECTING = 0; globalThis.WebSocket.OPEN = 1; globalThis.WebSocket.CLOSING = 2; globalThis.WebSocket.CLOSED = 3;
  }
  if (typeof globalThis.Headers !== "function") {
    def(globalThis, "Headers", function (init) { var m = {}; this.append = function (k, v) { m[String(k).toLowerCase()] = String(v); }; this.set = this.append; this.get = function (k) { var v = m[String(k).toLowerCase()]; return v === undefined ? null : v; }; this.has = function (k) { return String(k).toLowerCase() in m; }; this.delete = function (k) { delete m[String(k).toLowerCase()]; }; this.forEach = function (cb) { for (var k in m) { cb(m[k], k, this); } }; if (init) { for (var k in init) { if (Object.prototype.hasOwnProperty.call(init, k)) { this.append(k, init[k]); } } } });
  }

  // --- URLSearchParams ---------------------------------------------------------------------
  if (typeof globalThis.URLSearchParams !== "function") {
    def(globalThis, "URLSearchParams", function (init) {
      var pairs = [];
      function add(k, v) { pairs.push([String(k), String(v)]); }
      if (typeof init === "string") {
        var s = init.charAt(0) === "?" ? init.slice(1) : init;
        if (s) {
          var segs = s.split("&");
          for (var i = 0; i < segs.length; i++) {
            if (!segs[i]) { continue; }
            var eq = segs[i].indexOf("=");
            var k = eq < 0 ? segs[i] : segs[i].slice(0, eq);
            var v = eq < 0 ? "" : segs[i].slice(eq + 1);
            try { add(decodeURIComponent(k.replace(/\+/g, " ")), decodeURIComponent(v.replace(/\+/g, " "))); } catch (e) { add(k, v); }
          }
        }
      } else if (init && typeof init === "object") {
        if (typeof init.forEach === "function" && typeof init.length === "number") {
          for (var j = 0; j < init.length; j++) { add(init[j][0], init[j][1]); }
        } else {
          for (var key in init) { if (Object.prototype.hasOwnProperty.call(init, key)) { add(key, init[key]); } }
        }
      }
      this.append = function (k, v) { add(k, v); };
      this.set = function (k, v) { k = String(k); for (var i = pairs.length - 1; i >= 0; i--) { if (pairs[i][0] === k) { pairs.splice(i, 1); } } add(k, v); };
      this.get = function (k) { k = String(k); for (var i = 0; i < pairs.length; i++) { if (pairs[i][0] === k) { return pairs[i][1]; } } return null; };
      this.getAll = function (k) { k = String(k); var out = []; for (var i = 0; i < pairs.length; i++) { if (pairs[i][0] === k) { out.push(pairs[i][1]); } } return out; };
      this.has = function (k) { k = String(k); for (var i = 0; i < pairs.length; i++) { if (pairs[i][0] === k) { return true; } } return false; };
      this.delete = function (k) { k = String(k); for (var i = pairs.length - 1; i >= 0; i--) { if (pairs[i][0] === k) { pairs.splice(i, 1); } } };
      this.forEach = function (cb, thisArg) { for (var i = 0; i < pairs.length; i++) { cb.call(thisArg, pairs[i][1], pairs[i][0], this); } };
      this.keys = function () { return pairs.map(function (p) { return p[0]; }); };
      this.values = function () { return pairs.map(function (p) { return p[1]; }); };
      this.entries = function () { return pairs.slice(); };
      this.toString = function () { return pairs.map(function (p) { return encodeURIComponent(p[0]) + "=" + encodeURIComponent(p[1]); }).join("&"); };
      Object.defineProperty(this, "size", { get: function () { return pairs.length; }, enumerable: false, configurable: true });
    });
  }

  // --- URL ---------------------------------------------------------------------------------
  if (typeof globalThis.URL !== "function") {
    def(globalThis, "URL", function (url, base) {
      var resolved = String(url);
      // Very small relative-resolution against base origin+path.
      if (base != null && !/^[a-zA-Z][a-zA-Z0-9+.\-]*:/.test(resolved)) {
        var b = parseURL(String(base));
        var c0 = resolved.charCodeAt(0);
        if (c0 === 47) { resolved = b.origin + resolved; }            // '/'
        else if (c0 === 63) { resolved = b.origin + b.pathname + resolved; }  // '?'
        else if (c0 === 35) { resolved = b.origin + b.pathname + b.search + resolved; }  // '#'
        else { var dir = b.pathname.replace(/[^/]*$/, ""); resolved = b.origin + dir + resolved; }
      }
      var p = parseURL(resolved);
      this.href = p.href; this.protocol = p.protocol; this.host = p.host; this.hostname = p.hostname;
      this.port = p.port; this.pathname = p.pathname; this.search = p.search; this.hash = p.hash; this.origin = p.origin;
      this.username = ""; this.password = "";
      this.searchParams = new globalThis.URLSearchParams(p.search);
      this.toString = function () { return this.href; }; this.toJSON = function () { return this.href; };
    });
    globalThis.URL.createObjectURL = function () { return "blob:null/0"; };
    globalThis.URL.revokeObjectURL = fn;
  }
  if (typeof globalThis.queueMicrotask !== "function") { /* installed by timers */ }

  // --- misc presence stubs -----------------------------------------------------------------
  def(globalThis, "requestIdleCallback", function (cb) { return setTimeout(function () { try { cb({ didTimeout: false, timeRemaining: function () { return 0; } }); } catch (e) {} }, 1); });
  def(globalThis, "cancelIdleCallback", function (id) { return clearTimeout(id); });

  if (typeof globalThis.structuredClone !== "function") {
    def(globalThis, "structuredClone", function (v) { try { return JSON.parse(JSON.stringify(v)); } catch (e) { return v; } });
  }

  // base64 (btoa/atob) — pure JS implementation.
  var B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  def(globalThis, "btoa", function (input) {
    var str = String(input), out = "";
    for (var i = 0; i < str.length;) {
      var c1 = str.charCodeAt(i++) & 0xff;
      var c2 = str.charCodeAt(i++);
      var c3 = str.charCodeAt(i++);
      var e1 = c1 >> 2;
      var e2 = ((c1 & 3) << 4) | ((isNaN(c2) ? 0 : c2) >> 4);
      var e3 = isNaN(c2) ? 64 : (((c2 & 15) << 2) | ((isNaN(c3) ? 0 : c3) >> 6));
      var e4 = isNaN(c3) ? 64 : (c3 & 63);
      out += B64.charAt(e1) + B64.charAt(e2) + (e3 === 64 ? "=" : B64.charAt(e3)) + (e4 === 64 ? "=" : B64.charAt(e4));
    }
    return out;
  });
  def(globalThis, "atob", function (input) {
    // Drop whitespace; keep '=' padding so groups stay 4-aligned.
    var str = String(input).replace(/[^A-Za-z0-9+/=]/g, ""), out = "";
    for (var i = 0; i + 3 < str.length; i += 4) {
      var d1 = B64.indexOf(str.charAt(i));
      var d2 = B64.indexOf(str.charAt(i + 1));
      var p3 = str.charAt(i + 2), p4 = str.charAt(i + 3);
      var d3 = B64.indexOf(p3), d4 = B64.indexOf(p4);
      out += String.fromCharCode(((d1 << 2) | (d2 >> 4)) & 0xff);
      if (p3 !== "=" && d3 >= 0) { out += String.fromCharCode(((d2 & 15) << 4) | (d3 >> 2)); }
      if (p4 !== "=" && d4 >= 0) { out += String.fromCharCode(((d3 & 3) << 6) | d4); }
    }
    return out;
  });

  // crypto: no real RNG available; fill deterministically with a nonzero pattern.
  var cryptoSeed = 0x9e3779b9;
  function nextByte() { cryptoSeed = (cryptoSeed * 1103515245 + 12345) & 0x7fffffff; return ((cryptoSeed >> 16) & 0xff) || 1; }
  globalThis.crypto = {
    getRandomValues: function (arr) { if (arr && typeof arr.length === "number") { for (var i = 0; i < arr.length; i++) { arr[i] = nextByte(); } } return arr; },
    randomUUID: function () {
      var hex = "0123456789abcdef", s = "";
      for (var i = 0; i < 36; i++) {
        if (i === 8 || i === 13 || i === 18 || i === 23) { s += "-"; }
        else if (i === 14) { s += "4"; }
        else if (i === 19) { s += hex.charAt((nextByte() & 0x3) | 0x8); }
        else { s += hex.charAt(nextByte() & 0xf); }
      }
      return s;
    },
    subtle: {}
  };

  // fetch: present but rejects (no networking yet).
  if (typeof globalThis.fetch !== "function") {
    def(globalThis, "fetch", function () { return Promise.reject(new Error("fetch not implemented")); });
  }

  // XMLHttpRequest: present but inert.
  def(globalThis, "XMLHttpRequest", function () {
    this.readyState = 0; this.status = 0; this.responseText = ""; this.response = "";
    this.onreadystatechange = null; this.onload = null; this.onerror = null;
    this.open = fn; this.send = fn; this.setRequestHeader = fn; this.abort = fn;
    this.getResponseHeader = function () { return null; }; this.getAllResponseHeaders = function () { return ""; };
    this.addEventListener = fn; this.removeEventListener = fn;
  });

  // Constructors some pages feature-detect / construct.
  if (typeof globalThis.Event !== "function") {
    def(globalThis, "Event", function (type, init) { this.type = String(type); this.bubbles = !!(init && init.bubbles); this.cancelable = !!(init && init.cancelable); this.defaultPrevented = false; this.preventDefault = fn; this.stopPropagation = fn; });
  }
  if (typeof globalThis.CustomEvent !== "function") {
    def(globalThis, "CustomEvent", function (type, init) { this.type = String(type); this.detail = init ? init.detail : null; this.bubbles = !!(init && init.bubbles); this.preventDefault = fn; this.stopPropagation = fn; });
  }
})();
"#;

/// Install the browser environment APIs by:
/// 1. setting `globalThis.__pageURL` to `url` via a native string property (no JS-source
///    interpolation, so the URL can contain quotes/backslashes safely), then
/// 2. evaluating [`BROWSER_ENV_BOOTSTRAP`], which reads `__pageURL` to populate `location` and
///    the document URL fields and defines everything else.
///
/// Must be called *after* `install_globals`/`install_document`/`install_timers`: it overwrites
/// the minimal `location`, patches `document` (cookie, lifecycle, element enrichment), and uses
/// `setTimeout`/`__timerErrors` from the timer bootstrap.
fn install_browser_env(context: &mut Context, url: &str) {
    // Pass the URL as a real JS string value (not interpolated into source) to avoid any
    // quoting/injection issues with odd characters in the URL.
    context
        .register_global_property(
            js_string!("__pageURL"),
            JsValue::from(js_string!(url)),
            Attribute::all(),
        )
        .expect("register __pageURL global");
    context
        .eval(Source::from_bytes(BROWSER_ENV_BOOTSTRAP))
        .expect("install browser env bootstrap");
}

/// Maximum number of `__runDueTimers()` task iterations to run when draining the event loop.
/// Bounds runaway `setInterval`/self-rescheduling timers so the loop can never hang.
const EVENT_LOOP_CAP: usize = 10_000;

/// Drive the event loop to completion (or [`EVENT_LOOP_CAP`]) after all page sources have run.
///
/// Each iteration: (a) run Boa's pending promise jobs via `context.run_jobs()`, then (b) call
/// `globalThis.__runDueTimers()` and inspect its boolean result; stop when it returns `false`
/// (queue empty) or the cap is hit. Any `console.*` output produced by timer callbacks lands in
/// the shared `console` buffer; together with the JS-side `__timerErrors` array, it is folded
/// into `results` (appended to the last source's [`EvalOutput`], or a synthetic trailing one if
/// `results` is empty).
fn drain_event_loop(
    context: &mut Context,
    console: &Rc<RefCell<Vec<String>>>,
    results: &mut [EvalOutput],
) {
    // Console may carry leftovers from the last source's eval that were already attached; start
    // fresh so we only pick up output produced during the drain.
    console.borrow_mut().clear();

    // Fire the DOM lifecycle events (readystatechange/DOMContentLoaded/load) now that page
    // scripts have registered their handlers, so deferred init runs. Defined by the browser-env
    // bootstrap; a no-op (and harmless) on the non-DOM `eval_batch` path where it's absent.
    let _ = context.eval(Source::from_bytes(
        "if (typeof __fireLifecycleEvents === 'function') { __fireLifecycleEvents(); }",
    ));

    let mut iterations = 0usize;
    loop {
        if iterations >= EVENT_LOOP_CAP {
            break;
        }
        // Run any pending promise reaction jobs (e.g. resolved `Promise.then`). 0.21 exposes
        // `Context::run_jobs() -> JsResult<()>`; ignore its result so a rejected promise job
        // doesn't abort the drain.
        let _ = context.run_jobs();

        let ran = context.eval(Source::from_bytes("__runDueTimers()"));
        iterations += 1;
        match ran {
            Ok(v) if v.as_boolean() == Some(true) => continue,
            _ => break, // false, undefined, or an error → nothing left to do.
        }
    }

    // Collect any errors recorded by timer/microtask callbacks.
    let mut extra: Vec<String> = Vec::new();
    if let Ok(errs) = context.eval(Source::from_bytes(
        "(globalThis.__timerErrors || []).join('\\u0000')",
    )) {
        let joined = render_value(&errs, context);
        for e in joined.split('\u{0}') {
            if !e.is_empty() {
                extra.push(format!("⚠ {e}"));
            }
        }
    }

    // Console output produced during the drain (timer callbacks' `console.log`, etc.).
    let drained = std::mem::take(&mut *console.borrow_mut());

    if drained.is_empty() && extra.is_empty() {
        return;
    }

    // Fold into the last source's output, or a synthetic trailing one if there were no sources.
    if let Some(last) = results.last_mut() {
        last.console.extend(drained);
        last.console.extend(extra);
    }
    // When `results` is a borrowed slice we cannot push; the `eval_batch`/`run_with_dom` callers
    // always pass at least one result for non-empty source lists, and an empty source list has
    // no timers to drain — so the slice is only ever empty in the trivial no-op case.
}

/// Render a console argument to a reasonable string (numbers, strings, booleans, arrays…).
fn stringify_arg(value: &JsValue, context: &mut Context) -> String {
    render_value(value, context)
}

/// Render a `JsValue` to a display string, falling back to a coerced string conversion.
fn render_value(value: &JsValue, context: &mut Context) -> String {
    match value.to_string(context) {
        Ok(js_str) => js_str.to_std_string_escaped(),
        // `to_string` can throw (e.g. objects with a throwing `toString`); fall back.
        Err(_) => value.display().to_string(),
    }
}

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
        // Regression: Boa's recursive-descent parser overflowed a small thread stack on
        // deeply-nested real-world JS (e.g. youtube.com). `eval_batch` runs on a large stack
        // so this must not crash the process — it either parses or errors, but never faults.
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
        assert_eq!(out[0].value.as_deref(), Some(r#"<span class="hi">x</span>"#));
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

    #[test]
    fn document_title_returns_title_text() {
        let (doc, _) = doc_with_body("My Page");
        let (_doc, out) = run_with_dom(doc, vec!["document.title".to_string()], "https://example.com/");
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
        let (_doc, out) =
            run_with_dom(doc, vec![r#"setTimeout(() => console.log("tick"), 0);"#.to_string()], "https://example.com/");
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        let all: Vec<String> = out.iter().flat_map(|o| o.console.clone()).collect();
        assert!(all.iter().any(|l| l == "tick"), "expected 'tick' in {all:?}");
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
        assert!(fast < slow, "fast (10ms) must run before slow (50ms): {all:?}");
    }

    #[test]
    fn clear_timeout_cancels_callback() {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"var id = setTimeout(() => console.log("nope"), 0); clearTimeout(id);"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        let all: Vec<String> = out.iter().flat_map(|o| o.console.clone()).collect();
        assert!(!all.iter().any(|l| l == "nope"), "cancelled callback ran: {all:?}");
    }

    #[test]
    fn set_interval_is_bounded_and_does_not_hang() {
        // A self-perpetuating interval that never clears itself would loop forever without the
        // cap. The test completing (quickly) proves the cap works; the interval also has a
        // self-clearing variant below to verify ordinary intervals run.
        let (doc, _) = doc_with_body("");
        // An interval that never clears: it MUST be bounded by the cap, and the test must return.
        let (_doc, out) = run_with_dom(
            doc,
            vec![r#"globalThis.n = 0; setInterval(() => { globalThis.n++; if (globalThis.n === 12) console.log("ran " + globalThis.n); }, 1);"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        // The interval ran at least 12 times (logged during the drain) before the cap halted it.
        let all: Vec<String> = out.iter().flat_map(|o| o.console.clone()).collect();
        assert!(all.iter().any(|l| l == "ran 12"), "interval did not run repeatedly: {all:?}");

        // A self-clearing interval: stop after 3 ticks. Should not be cut off by the cap.
        let (doc2, _) = doc_with_body("");
        let (_doc2, out2) = run_with_dom(
            doc2,
            vec![r#"var k = 0; var h = setInterval(() => { k++; console.log("k" + k); if (k >= 3) clearInterval(h); }, 5);"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out2[0].error, None, "{:?}", out2[0]);
        let all2: Vec<String> = out2.iter().flat_map(|o| o.console.clone()).collect();
        assert_eq!(
            all2.iter().filter(|l| l.starts_with('k')).count(),
            3,
            "self-clearing interval should tick exactly 3 times: {all2:?}"
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
        assert!(all.iter().any(|l| l == "after"), "loop died on throw: {all:?}");
        // The error surfaced (prefixed with the warning marker).
        assert!(all.iter().any(|l| l.contains('⚠') && l.contains("boom")), "error not reported: {all:?}");
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
        assert!(!all.iter().any(|l| l == "cancelled"), "cancelAnimationFrame failed: {all:?}");
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

    // --- Browser environment (`install_browser_env`) ------------------------------------

    /// Convenience: run one expression source against a fresh doc+body at the given URL and
    /// return its [`EvalOutput`].
    fn env_eval(url: &str, src: &str) -> EvalOutput {
        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_with_dom(doc, vec![src.to_string()], url);
        out.into_iter().next().unwrap()
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
    fn get_computed_style_returns_empty_strings() {
        let out = env_eval(
            "https://example.com/",
            "getComputedStyle(document.body).getPropertyValue('color') === '' \
             && getComputedStyle(document.body).color === ''",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("true"));
    }

    #[test]
    fn add_event_listener_exists_on_window_and_document() {
        let out = env_eval(
            "https://example.com/",
            "[typeof window.addEventListener, typeof document.addEventListener, \
              typeof window.dispatchEvent, typeof document.removeEventListener].join(',')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("function,function,function,function"));
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
        assert!(all.iter().any(|l| l == "dcl-fired"), "DOMContentLoaded did not fire: {all:?}");
        assert!(all.iter().any(|l| l == "load-fired"), "load did not fire: {all:?}");
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
    fn crypto_get_random_values_fills_nonzero() {
        let out = env_eval(
            "https://example.com/",
            "var a = new Uint8Array(4); crypto.getRandomValues(a); \
             a.every(function (x) { return x !== 0; })",
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
            vec![r#"document.body.style.display = "none"; document.body.style.display"#.to_string()],
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
        assert!(style.contains("background-color: red"), "style attr was {style:?}");
    }

    #[test]
    fn style_reads_existing_style_attribute() {
        // Pre-seed a style="" attribute and confirm el.style reads from it.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let html = doc.append_element(root, "html");
        let body = doc.append_element(html, "body");
        if let dom::NodeData::Element(e) = &mut doc.get_mut(body).data {
            e.attrs.insert("style".into(), "display: none; color: blue".into());
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
        let out = env_eval(
            "https://example.com/",
            "var r = document.body.getBoundingClientRect(); \
             [r.x, r.y, r.top, r.left, r.right, r.bottom, r.width, r.height].join(',')",
        );
        assert_eq!(out.error, None, "{out:?}");
        assert_eq!(out.value.as_deref(), Some("0,0,0,0,0,0,0,0"));
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
    fn navigation_accessors_and_enrichment_propagate() {
        // A child reached via navigation must itself be enriched (style write-through works).
        let mut doc = dom::Document::new();
        let root = doc.root();
        let html = doc.append_element(root, "html");
        let body = doc.append_element(html, "body");
        let child = doc.append_element(body, "div");
        let (doc, out) = run_with_dom(
            doc,
            vec![r#"var c = document.body.firstElementChild; c.style.display = "block"; c.tagName"#
                .to_string()],
            "https://example.com/",
        );
        assert_eq!(out[0].error, None, "{:?}", out[0]);
        assert_eq!(out[0].value.as_deref(), Some("DIV"));
        let style = attr_of(&doc, child, "style").unwrap_or_default();
        assert!(style.contains("display: block"), "child style attr was {style:?}");
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
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules);
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(console.iter().any(|l| l == "got 42"), "console was {console:?}");
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
        modules.insert(leaf, r#"export function hello() { return "chained"; }"#.to_string());

        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules);
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(console.iter().any(|l| l == "chained"), "console was {console:?}");
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
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules);
        // Must not panic; the entry's evaluation surfaces an error.
        assert!(out.iter().any(|o| o.error.is_some()), "expected an error, got {out:?}");
    }

    #[test]
    fn side_effect_import_runs_imported_module() {
        let entry = "https://x/app.js".to_string();
        let dep = "https://x/dep.js".to_string();
        let mut modules = std::collections::HashMap::new();
        modules.insert(entry.clone(), r#"import "https://x/dep.js";"#.to_string());
        modules.insert(dep, r#"console.log("side effect ran");"#.to_string());

        let (doc, _) = doc_with_body("");
        let (_doc, out) = run_modules(doc, "https://x/", vec![entry], modules);
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(
            console.iter().any(|l| l == "side effect ran"),
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
            r#"document.title = "from-module"; console.log("title:" + document.title);"#.to_string(),
        );

        let (doc, _) = doc_with_body("orig");
        let (doc, out) = run_modules(doc, "https://x/", vec![entry], modules);
        let console = all_console(&out);
        assert!(out.iter().all(|o| o.error.is_none()), "errors: {out:?}");
        assert!(console.iter().any(|l| l == "title:from-module"), "console was {console:?}");
        // The mutation is visible in the returned document.
        let title = find_by_tag(&doc, doc.root(), "title").map(|n| text_content(&doc, n));
        assert_eq!(title.as_deref(), Some("from-module"));
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
            "#.to_string()],
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
}
