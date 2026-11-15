use crate::*;

/// Bound on distinct fetched modules, shared by static and dynamic imports.
pub(crate) const MODULE_CAP: usize = 800;

/// URL source cache; Lumen owns parsing, module identities, linking and evaluation.
pub(crate) struct ModuleRegistry {
    sources: RefCell<HashMap<String, String>>,
    fetcher: Rc<dyn Fn(&str) -> Option<(String, String)>>,
    base_url: String,
}

impl ModuleRegistry {
    fn resolve_specifier(specifier: &str, base: &str) -> String {
        wurl::resolve(specifier, base).unwrap_or_else(|| specifier.to_string())
    }

    fn source_for(&self, url: &str) -> Option<String> {
        if let Some(source) = self.sources.borrow().get(url) {
            return Some(source.clone());
        }
        if self.sources.borrow().len() >= MODULE_CAP {
            return None;
        }
        let source = (self.fetcher)(url)?.0;
        self.sources
            .borrow_mut()
            .insert(url.to_string(), source.clone());
        Some(source)
    }

    fn load(
        &self,
        specifier: &str,
        referrer: &str,
        attr: Option<&str>,
    ) -> Option<(String, String)> {
        let base = if referrer.is_empty() {
            &self.base_url
        } else {
            referrer
        };
        let key = Self::resolve_specifier(specifier, base);
        self.source_for(&key).map(|source| {
            let source = if attr.is_none() {
                prepare_module_source(&source)
            } else {
                source
            };
            (key, source)
        })
    }
}

/// Lumen supplies import.meta.url; the browser also supplies the URL resolver required by HTML.
/// Build the resolver in the global environment and capture the module URL explicitly: author
/// bindings named `globalThis`, `URL`, or `Function` must not shadow the resolver's intrinsics.
fn prepare_module_source(source: &str) -> String {
    const PRELUDE: &str =
        "import.meta.resolve = (function(){}).constructor('base', 'return function(specifier) { return new globalThis.URL(specifier, base).href; };')(import.meta.url);\n";
    if source.starts_with("#!") {
        let split = source.find('\n').unwrap_or(source.len());
        format!("{}\n{PRELUDE}{}", &source[..split], &source[split..])
    } else {
        format!("{PRELUDE}{source}")
    }
}

/// Install one loader for static dependencies and later dynamic imports.
pub(crate) fn install_module_loader(
    engine: &mut Engine,
    sources: HashMap<String, String>,
    fetcher: Rc<dyn Fn(&str) -> Option<(String, String)>>,
    base_url: &str,
) -> Rc<ModuleRegistry> {
    let registry = Rc::new(ModuleRegistry {
        sources: RefCell::new(sources),
        fetcher,
        base_url: base_url.to_string(),
    });
    let loader = registry.clone();
    engine.set_import_base(base_url);
    engine.set_module_loader_attrs(move |specifier, referrer, attr| {
        loader.load(specifier, referrer, attr)
    });
    registry
}

/// Evaluate one entry with Lumen's native module implementation, including top-level await.
pub(crate) fn run_one_entry(
    engine: &mut Engine,
    registry: &Rc<ModuleRegistry>,
    entry: &str,
) -> EvalOutput {
    let state = host_state(engine.ctx());
    state.console.borrow_mut().clear();
    let error = match registry.source_for(entry) {
        None => Some(format!("entry module not found: {entry}")),
        Some(source) => {
            let loader = registry.clone();
            match engine.eval_module_attrs(
                &prepare_module_source(&source),
                entry,
                move |specifier, referrer, attr| loader.load(specifier, referrer, attr),
            ) {
                Ok(lumen::Completion::Value(_)) => None,
                Ok(lumen::Completion::Throw { name, message }) => Some(if name.is_empty() {
                    message
                } else {
                    format!("{name}: {message}")
                }),
                Err(error) => Some(format!(
                    "SyntaxError: {} (line {})",
                    error.message, error.line
                )),
            }
        }
    };
    let console = std::mem::take(&mut *state.console.borrow_mut());
    EvalOutput {
        value: None,
        console,
        error,
    }
}

/// Run a page's module graph in the same browser environment used by persistent sessions.
pub fn run_modules(
    doc: dom::Document,
    url: &str,
    entries: Vec<String>,
    modules: HashMap<String, String>,
    fetcher: Box<dyn Fn(&str) -> Option<(String, String)> + Send>,
    request_fetcher: Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync>,
    cookie_getter: Arc<dyn Fn(&str) -> String + Send + Sync>,
    cookie_setter: Arc<dyn Fn(&str, &str) -> bool + Send + Sync>,
) -> (dom::Document, Vec<EvalOutput>) {
    let (session, snapshot, outputs) = Session::new(
        doc,
        Vec::new(),
        entries,
        modules,
        url,
        fetcher,
        request_fetcher,
        Arc::new(|_, _, _| Err("no WebSocket connector".to_string())),
        cookie_getter,
        cookie_setter,
        None,
    );
    drop(session);
    (snapshot, outputs)
}

/// Escape a string for embedding inside a double-quoted JS string literal.
pub(crate) fn js_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    fn module_source(name: &str) -> Option<(String, String)> {
        let source = match name {
            // b completes registration through the cycle before a's other branch fails.
            "a" => "import 'b'; import 'tail'; globalThis.executed = true;",
            "b" => "import 'a'; export const b = 1;",
            "tail" => "import 'missing';",
            "second" => "import 'b';",
            _ => return None,
        };
        Some((name.into(), source.into()))
    }

    fn assert_missing_module(completion: lumen::Completion) {
        match completion {
            lumen::Completion::Throw { name, message } => {
                assert_eq!(name, "TypeError");
                assert!(message.contains("missing"), "{message}");
            }
            lumen::Completion::Value(value) => panic!("expected failed import, got value {value}"),
        }
    }

    #[test]
    fn failed_module_graph_cannot_poison_later_static_or_dynamic_imports() {
        let mut engine = lumen::Engine::new();
        for name in ["a", "second", "b", "a", "second"] {
            let source = module_source(name).unwrap().1;
            assert_missing_module(
                engine
                    .eval_module(&source, name, |name, _| module_source(name))
                    .unwrap(),
            );
        }
        engine.eval("var result = 'pending'; import('b').then(() => result = 'resolved', error => result = error.name);", false).unwrap();
        assert!(
            matches!(engine.eval("result", false).unwrap(), lumen::Completion::Value(value) if value == "TypeError")
        );
        assert!(
            matches!(engine.eval("typeof executed", false).unwrap(), lumen::Completion::Value(value) if value == "undefined")
        );
        assert!(matches!(
            engine
                .eval_module("globalThis.recovered = 42;", "valid", |_, _| None)
                .unwrap(),
            lumen::Completion::Value(_)
        ));
        assert!(
            matches!(engine.eval("recovered", false).unwrap(), lumen::Completion::Value(value) if value == "42")
        );
    }

    #[test]
    fn failed_link_in_cycle_stays_failed_when_imported_through_another_member() {
        fn source(name: &str) -> Option<(String, String)> {
            Some((
                name.into(),
                match name {
                    "a" => "import 'b'; export { absent } from 'b'; globalThis.executed = true;",
                    "b" => "import 'a'; export const present = 1;",
                    _ => return None,
                }
                .into(),
            ))
        }
        let mut engine = lumen::Engine::new();
        for name in ["a", "b", "a", "b"] {
            let result = engine
                .eval_module(&source(name).unwrap().1, name, |name, _| source(name))
                .unwrap();
            assert!(
                matches!(result, lumen::Completion::Throw { name, .. } if name == "SyntaxError")
            );
        }
        assert!(
            matches!(engine.eval("typeof executed", false).unwrap(), lumen::Completion::Value(value) if value == "undefined")
        );
    }
}
