use crate::*;

/// One executable script slot in document order: an inline `<script>` body, or an external
/// `<script src>` whose `src` resolved to an absolute URL. Pure classification, no fetching.
#[derive(Debug, PartialEq, Eq)]
pub enum ScriptSource {
    Inline(String),
    External(String),
}

/// Walk the DOM in document order, classifying each *runnable* `<script>` element. Inline
/// scripts contribute their text body; `<script src>` contribute the resolved absolute URL.
/// Scripts with a non-JS `type` (e.g. `application/json`, `application/ld+json`) are omitted.
/// `<script type="module">` is also skipped here — modules are collected separately by
/// [`collect_module_entries`] and run (deferred) via [`run_modules`]. Pure: unit-testable
/// without network.
pub fn collect_script_sources(doc: &dom::Document, base: &str) -> Vec<ScriptSource> {
    fn is_js_type(ty: Option<&str>) -> bool {
        match ty {
            None => true,
            Some(t) => {
                let t = t.trim().to_ascii_lowercase();
                t.is_empty()
                    || t == "text/javascript"
                    || t == "application/javascript"
                    || t == "text/ecmascript"
                    || t == "application/ecmascript"
            }
        }
    }
    fn walk(doc: &dom::Document, id: dom::NodeId, base: &str, out: &mut Vec<ScriptSource>) {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag == "script" {
                if is_js_type(e.attrs.get("type").map(String::as_str)) {
                    if let Some(src) = e.attrs.get("src") {
                        if let Some(abs) = resolve_url(base, src) {
                            out.push(ScriptSource::External(abs));
                        }
                    } else {
                        // The HTML parser stores a script's body as a single Text child.
                        let mut source = String::new();
                        for &child in &doc.get(id).children {
                            if let dom::NodeData::Text(t) = &doc.get(child).data {
                                source.push_str(t);
                            }
                        }
                        out.push(ScriptSource::Inline(source));
                    }
                }
                // Don't descend into a script's children (its text body isn't markup).
                return;
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

/// Maximum number of modules fetched per page's module graph (across all entries).
pub(crate) const MAX_MODULES: usize = 400;
/// Skip module sources larger than this (Vue's runtime is ~400 KiB; 16 MiB is generous).
pub(crate) const MAX_MODULE_BYTES: usize = 16 * 1024 * 1024;

/// One ES-module entry point in document order: an inline `<script type=module>` body (with the
/// page URL as its base), or an external `<script type=module src>` whose `src` resolved to an
/// absolute URL. Pure classification, no fetching.
#[derive(Debug, PartialEq, Eq)]
pub enum ModuleEntry {
    /// Inline module source. The base URL for resolving its imports is the page URL.
    Inline(String),
    /// External module URL (already resolved to an absolute `http(s)`/`file` URL).
    External(String),
}

/// Walk the DOM in document order, collecting `<script type="module">` elements (the ones
/// [`collect_script_sources`] deliberately skips). Inline modules contribute their text body;
/// external `<script type=module src>` contribute the resolved absolute URL. Pure: unit-testable
/// without network.
pub fn collect_module_entries(doc: &dom::Document, base: &str) -> Vec<ModuleEntry> {
    fn is_module_type(ty: Option<&str>) -> bool {
        matches!(ty, Some(t) if t.trim().eq_ignore_ascii_case("module"))
    }
    fn walk(doc: &dom::Document, id: dom::NodeId, base: &str, out: &mut Vec<ModuleEntry>) {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag == "script" {
                if is_module_type(e.attrs.get("type").map(String::as_str)) {
                    if let Some(src) = e.attrs.get("src") {
                        if let Some(abs) = resolve_url(base, src) {
                            out.push(ModuleEntry::External(abs));
                        }
                    } else {
                        let mut source = String::new();
                        for &child in &doc.get(id).children {
                            if let dom::NodeData::Text(t) = &doc.get(child).data {
                                source.push_str(t);
                            }
                        }
                        out.push(ModuleEntry::Inline(source));
                    }
                }
                return;
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

/// One import/export specifier found in a module's source: the byte range of the *quoted* string
/// literal (including its quotes) plus the unquoted specifier text. Used to both resolve and
/// rewrite specifiers in place.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SpecifierRef {
    /// Byte offset of the opening quote in the source.
    pub(crate) start: usize,
    /// Byte offset just past the closing quote.
    pub(crate) end: usize,
    /// The specifier string between the quotes (no quotes).
    pub(crate) spec: String,
}

/// Tolerantly scan `src` for static, string-literal module specifiers in `import`/`export`
/// statements and dynamic `import(...)` calls. Recognizes:
///   - `import ... from 'spec'` / `import ... from "spec"`
///   - `import 'spec'` (side-effect)
///   - `export ... from 'spec'` / `export * from 'spec'`
///   - `import('spec')` (dynamic, string-literal argument only)
///
/// This is a lexical scan, not a full parse: it skips line/block comments and string/template
/// literals so the keywords/quotes inside them aren't mistaken for imports, then looks for the
/// `from` / bare-import / `import(` patterns. Only static string literals are returned;
/// computed dynamic imports (`import(expr)`) are ignored. Never panics.
pub(crate) fn extract_specifiers(src: &str) -> Vec<SpecifierRef> {
    let b = src.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut i = 0usize;

    // Read a quoted string literal starting at the opening quote `b[i]`; returns (spec, end).
    fn read_string(b: &[u8], i: usize) -> Option<(String, usize)> {
        let quote = b[i];
        let mut j = i + 1;
        let mut s = Vec::new();
        while j < b.len() {
            let c = b[j];
            if c == b'\\' {
                // Keep escapes verbatim; specifiers rarely use them and we only need the URL form.
                if j + 1 < b.len() {
                    s.push(b[j + 1]);
                    j += 2;
                    continue;
                }
                return None;
            }
            if c == quote {
                return Some((String::from_utf8_lossy(&s).into_owned(), j + 1));
            }
            if c == b'\n' {
                return None; // unterminated single-line string
            }
            s.push(c);
            j += 1;
        }
        None
    }

    // Is the byte at `p` a JS identifier char (so we can require word boundaries around keywords)?
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';

    while i < n {
        let c = b[i];
        // Skip comments.
        if c == b'/' && i + 1 < n && b[i + 1] == b'/' {
            i += 2;
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        // Skip string / template literals (so their contents aren't scanned for keywords).
        if c == b'"' || c == b'\'' {
            match read_string(b, i) {
                Some((_, end)) => {
                    i = end;
                    continue;
                }
                None => {
                    i += 1;
                    continue;
                }
            }
        }
        if c == b'`' {
            // Template literal: skip to the matching backtick, honoring escapes. Nested `${}` may
            // contain backticks; we don't fully track them, but mismatches only cause us to miss a
            // specifier, never to misrewrite one.
            let mut j = i + 1;
            while j < n {
                if b[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if b[j] == b'`' {
                    break;
                }
                j += 1;
            }
            i = (j + 1).min(n);
            continue;
        }

        // Match `import` or `export` keyword at a word boundary.
        let is_import = b[i..].starts_with(b"import");
        let word = if is_import || b[i..].starts_with(b"export") {
            Some(6)
        } else {
            None
        };
        if let Some(kw_len) = word {
            let before_ok = i == 0 || !is_ident(b[i - 1]);
            let after = i + kw_len;
            let after_ok = after >= n || !is_ident(b[after]);
            if before_ok && after_ok {
                // Dynamic `import(...)`: skip whitespace after the keyword, expect `(`.
                if is_import {
                    let mut k = after;
                    while k < n && b[k].is_ascii_whitespace() {
                        k += 1;
                    }
                    if k < n && b[k] == b'(' {
                        k += 1;
                        while k < n && b[k].is_ascii_whitespace() {
                            k += 1;
                        }
                        if k < n && (b[k] == b'"' || b[k] == b'\'') {
                            if let Some((spec, end)) = read_string(b, k) {
                                out.push(SpecifierRef {
                                    start: k,
                                    end,
                                    spec,
                                });
                                i = end;
                                continue;
                            }
                        }
                        i = after;
                        continue;
                    }
                }
                // Static import/export. For a bare side-effect `import 'spec'`, the next non-space
                // token is the string itself. Otherwise the specifier follows a `from` keyword,
                // bounded by the statement terminator (`;`) or end of source.
                if is_import {
                    let mut p = after;
                    while p < n && b[p].is_ascii_whitespace() {
                        p += 1;
                    }
                    if p < n && (b[p] == b'"' || b[p] == b'\'') {
                        if let Some((spec, end)) = read_string(b, p) {
                            out.push(SpecifierRef {
                                start: p,
                                end,
                                spec,
                            });
                            i = end;
                            continue;
                        }
                    }
                }
                // Scan to a `from` keyword (bounded by the next `;`), then read the string after it.
                let stmt_end = b[after..]
                    .iter()
                    .position(|&c| c == b';')
                    .map(|off| after + off)
                    .unwrap_or(n);
                let mut k = after;
                let mut matched = false;
                while k < stmt_end {
                    if b[k..].starts_with(b"from")
                        && (k == 0 || !is_ident(b[k - 1]))
                        && (k + 4 >= n || !is_ident(b[k + 4]))
                    {
                        let mut p = k + 4;
                        while p < stmt_end && b[p].is_ascii_whitespace() {
                            p += 1;
                        }
                        if p < stmt_end && (b[p] == b'"' || b[p] == b'\'') {
                            if let Some((spec, end)) = read_string(b, p) {
                                out.push(SpecifierRef {
                                    start: p,
                                    end,
                                    spec,
                                });
                                i = end;
                                matched = true;
                                break;
                            }
                        }
                    }
                    k += 1;
                }
                if !matched {
                    i = after;
                }
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Classify a specifier for module resolution.
pub(crate) fn is_bare_specifier(spec: &str) -> bool {
    let s = spec.trim();
    if s.starts_with("./") || s.starts_with("../") || s.starts_with('/') {
        return false;
    }
    // A scheme like `http:`/`https:`/`file:` makes it absolute, not bare.
    !matches!(
        s.split_once(':'),
        Some((scheme, _)) if scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
            && scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
    )
}

/// Build the page's ES-module graph by fetching the entries and every transitively-imported
/// module, rewriting each module's import/export specifiers to the canonical absolute URL it was
/// fetched under. Returns the entry canonical URLs (document order), a `url -> rewritten source`
/// map, and console notes for skipped bare imports / failed loads.
///
/// `inline_counter` produces a synthetic unique URL for each inline `<script type=module>` so the
/// loader can key it; its imports resolve against the page URL.
pub fn collect_module_graph(
    doc: &dom::Document,
    page_url: &str,
) -> (Vec<String>, HashMap<String, String>, Vec<String>) {
    let entries_raw = collect_module_entries(doc, page_url);
    if entries_raw.is_empty() {
        return (Vec::new(), HashMap::new(), Vec::new());
    }

    let mut sources: HashMap<String, String> = HashMap::new();
    let mut notes: Vec<String> = Vec::new();
    let mut entry_urls: Vec<String> = Vec::new();
    // Work queue of (canonical url, base url for resolving its imports) modules to process.
    let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    // Raw (un-rewritten) sources fetched so far, keyed by canonical url, with the base for imports.
    let mut raw: HashMap<String, String> = HashMap::new();

    let mut inline_idx = 0usize;
    for entry in entries_raw {
        match entry {
            ModuleEntry::Inline(src) => {
                let url = format!("{page_url}#inline-module-{inline_idx}");
                inline_idx += 1;
                entry_urls.push(url.clone());
                raw.insert(url.clone(), src);
                queue.push_back(url);
            }
            ModuleEntry::External(url) => {
                if entry_urls.contains(&url) || raw.contains_key(&url) {
                    if !entry_urls.contains(&url) {
                        entry_urls.push(url);
                    }
                    continue;
                }
                entry_urls.push(url.clone());
                queue.push_back(url);
            }
        }
    }

    // BFS the graph level-by-level, fetching each level's modules CONCURRENTLY (a Vue app pulls
    // 200+ modules — sequential fetch dominated load time). `seen` dedups everything ever queued.
    let mut seen: std::collections::HashSet<String> = entry_urls.iter().cloned().collect();
    let mut frontier: Vec<String> = queue.into_iter().collect();

    while !frontier.is_empty() {
        // Cap the total module count, trimming this level's overflow with a note.
        let remaining = MAX_MODULES.saturating_sub(sources.len());
        if remaining == 0 {
            for u in &frontier {
                notes.push(format!(
                    "[skipped module (limit {MAX_MODULES} reached): {u}]"
                ));
            }
            break;
        }
        if frontier.len() > remaining {
            for u in frontier.split_off(remaining) {
                notes.push(format!(
                    "[skipped module (limit {MAX_MODULES} reached): {u}]"
                ));
            }
        }

        // Separate inline sources (already in `raw`) from network URLs to fetch.
        let mut bodies: Vec<(String, Result<String, String>)> = Vec::new();
        let mut net_urls: Vec<String> = Vec::new();
        for url in frontier.drain(..) {
            if let Some(src) = raw.remove(&url) {
                bodies.push((url, Ok(src)));
            } else if !url.contains("#inline-module-") {
                net_urls.push(url);
            }
        }

        // Fetch this level concurrently across a small scoped thread pool.
        if !net_urls.is_empty() {
            let n = net_urls.len().clamp(1, 8);
            let mut chunks: Vec<Vec<String>> = (0..n).map(|_| Vec::new()).collect();
            for (i, u) in net_urls.into_iter().enumerate() {
                chunks[i % n].push(u);
            }
            std::thread::scope(|s| {
                let handles: Vec<_> = chunks
                    .into_iter()
                    .map(|chunk| {
                        s.spawn(move || {
                            chunk
                                .into_iter()
                                .map(|u| {
                                    let r = match net::fetch(&u) {
                                        Ok(resp) if resp.body.len() > MAX_MODULE_BYTES => {
                                            Err(format!(
                                                "[skipped large module: {} ({} bytes)]",
                                                u,
                                                resp.body.len()
                                            ))
                                        }
                                        Ok(resp) => {
                                            Ok(String::from_utf8_lossy(&resp.body).into_owned())
                                        }
                                        Err(e) => {
                                            Err(format!("[failed to load module: {u} — {e}]"))
                                        }
                                    };
                                    (u, r)
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                for h in handles {
                    bodies.extend(h.join().unwrap_or_default());
                }
            });
        }

        // Process each fetched module: rewrite specifiers to canonical URLs, discover next level.
        let mut next: Vec<String> = Vec::new();
        for (url, body_res) in bodies {
            let body = match body_res {
                Ok(b) => b,
                Err(note) => {
                    notes.push(note);
                    continue;
                }
            };
            // Imports resolve against the page URL for inline entries, else the module's own URL.
            let base = if url.contains("#inline-module-") {
                page_url.to_string()
            } else {
                url.clone()
            };
            let specs = extract_specifiers(&body);
            let mut replacements: Vec<(usize, usize, String)> = Vec::new();
            for sp in &specs {
                if is_bare_specifier(&sp.spec) {
                    notes.push(format!("[skipped bare import: {}]", sp.spec));
                    continue;
                }
                let resolved = match wurl::resolve(sp.spec.trim(), &base) {
                    Some(u) => u,
                    None => {
                        notes.push(format!(
                            "[failed to resolve import: {} (in {url})]",
                            sp.spec
                        ));
                        continue;
                    }
                };
                let scheme = resolved.split(':').next().unwrap_or("");
                if !matches!(scheme, "http" | "https" | "file") {
                    notes.push(format!("[skipped non-loadable import: {}]", sp.spec));
                    continue;
                }
                let quote = &body[sp.start..sp.start + 1];
                replacements.push((sp.start, sp.end, format!("{quote}{resolved}{quote}")));
                if !seen.contains(&resolved) {
                    seen.insert(resolved.clone());
                    next.push(resolved);
                }
            }
            replacements.sort_by_key(|r| std::cmp::Reverse(r.0));
            let mut rewritten = body;
            for (start, end, rep) in replacements {
                rewritten.replace_range(start..end, &rep);
            }
            sources.insert(url, rewritten);
        }
        frontier = next;
    }

    (entry_urls, sources, notes)
}

/// Run the page's ES modules (deferred — after classic scripts). Builds the module graph
/// (fetch + rewrite via [`collect_module_graph`]) and executes it through [`js::run_modules`] so
/// modules share the same DOM-wired `document`/`window` classic scripts use. Returns the mutated
/// document plus console/error/note lines (errors prefixed `⚠`).
pub fn run_modules(doc: dom::Document, page_url: &str) -> (dom::Document, Vec<String>) {
    let (entries, sources, notes) = collect_module_graph(&doc, page_url);
    if entries.is_empty() {
        return (doc, notes);
    }
    // On-demand fetcher for dynamic imports of modules not in the pre-fetched static graph.
    // Called only on the JS isolate's own worker thread, so blocking `net::fetch` is fine here.
    let fetcher: Box<dyn Fn(&str) -> Option<(String, String)> + Send> = Box::new(|u: &str| {
        net::fetch(u).ok().map(|r| {
            (
                String::from_utf8_lossy(&r.body).into_owned(),
                r.content_type,
            )
        })
    });
    let request_fetcher = build_request_fetcher();
    let cookie_getter = build_cookie_getter();
    let cookie_setter = build_cookie_setter();
    let (doc, results) = js::run_modules(
        doc,
        page_url,
        entries,
        sources,
        fetcher,
        request_fetcher,
        cookie_getter,
        cookie_setter,
    );
    let mut out = notes;
    for result in results {
        out.extend(result.console);
        if let Some(err) = result.error {
            out.push(format!("⚠ {err}"));
        }
    }
    (doc, out)
}

/// Collect the page's scripts in document order — inline `<script>` bodies and external
/// `<script src>` (fetched against `base`) — and run them all on a single shared [`js`]
/// context (so later scripts see earlier globals AND each other's DOM mutations). Returns the
/// mutated document plus all captured console lines and any error lines (prefixed `⚠`).
/// Failed/over-limit/too-large external fetches contribute a `[…]` note in document order.
/// External fetches are sequential (classic blocking-script order); correctness over speed.
///
/// Takes the document by value and returns it: the JS path needs to *own* the tree to mutate
/// it (e.g. `el.textContent = "..."`), so the returned, possibly-mutated document is what the
/// caller should store and render.
pub fn run_scripts(doc: dom::Document, base: &str) -> (dom::Document, Vec<String>) {
    let classified = collect_script_sources(&doc, base);
    if classified.is_empty() {
        return (doc, Vec::new());
    }

    // Per-slot outcome in document order: either an executed source (indexed into `sources`)
    // or a pre-formatted skip/failure note to emit verbatim.
    enum Slot {
        Source(usize),
        Note(String),
    }
    let mut slots = Vec::new();
    let mut sources: Vec<String> = Vec::new();
    let mut fetched = 0usize;
    for item in classified {
        match item {
            ScriptSource::Inline(src) => {
                if src.len() > MAX_SCRIPT_BYTES {
                    slots.push(Slot::Note(format!(
                        "[skipped large script: {} bytes]",
                        src.len()
                    )));
                } else {
                    slots.push(Slot::Source(sources.len()));
                    sources.push(src);
                }
            }
            ScriptSource::External(url) => {
                if fetched >= MAX_EXTERNAL_SCRIPTS {
                    slots.push(Slot::Note(format!(
                        "[skipped script (limit {MAX_EXTERNAL_SCRIPTS} reached): {url}]"
                    )));
                    continue;
                }
                fetched += 1;
                match net::fetch(&url) {
                    Ok(resp) if resp.body.len() > MAX_SCRIPT_BYTES => {
                        slots.push(Slot::Note(format!(
                            "[skipped large script: {} ({} bytes)]",
                            url,
                            resp.body.len()
                        )))
                    }
                    Ok(resp) => {
                        slots.push(Slot::Source(sources.len()));
                        sources.push(String::from_utf8_lossy(&resp.body).into_owned());
                    }
                    Err(e) => {
                        slots.push(Slot::Note(format!("[failed to load script: {url} — {e}]")))
                    }
                }
            }
        }
    }

    // Execute all sources on one DOM-aware context (off-thread, large stack) in document order
    // so later scripts see earlier globals and DOM mutations. Returns the mutated document.
    let (doc, results) = if sources.is_empty() {
        (doc, Vec::new())
    } else {
        js::run_with_dom(doc, sources, base)
    };

    let mut out = Vec::new();
    for slot in slots {
        match slot {
            Slot::Source(i) => {
                if let Some(result) = results.get(i) {
                    out.extend(result.console.iter().cloned());
                    if let Some(err) = &result.error {
                        out.push(format!("⚠ {err}"));
                    }
                }
            }
            Slot::Note(note) => out.push(note),
        }
    }
    (doc, out)
}

/// Gather the page's classic script sources (inline + external `<script src>`) and its ES-module
/// graph, then start a persistent [`js::Session`] that runs them and stays alive for interactivity.
/// Returns the session (None if the page has no scripts/modules), the initial DOM snapshot, and
/// console/error/note lines. Mirrors the gathering in [`run_scripts`]/[`run_modules`] but hands the
/// work to a long-lived runtime instead of a run-once worker.
#[allow(clippy::type_complexity)]
pub(crate) fn start_session(
    doc: dom::Document,
    base: &str,
    initial_rects: Option<(
        Vec<(usize, f32, f32, f32, f32)>,
        Vec<(usize, f32, f32)>,
        Vec<(usize, f32, f32, f32, f32)>,
        Vec<(usize, f32, f32, f32, f32)>,
        f32,
        f32,
    )>,
) -> (Option<js::Session>, dom::Document, Vec<String>) {
    let mut notes: Vec<String> = Vec::new();

    // Classic scripts, in document order.
    let mut classic: Vec<String> = Vec::new();
    let mut fetched = 0usize;
    for item in collect_script_sources(&doc, base) {
        match item {
            ScriptSource::Inline(src) => {
                if src.len() > MAX_SCRIPT_BYTES {
                    notes.push(format!("[skipped large script: {} bytes]", src.len()));
                } else {
                    classic.push(src);
                }
            }
            ScriptSource::External(url) => {
                if fetched >= MAX_EXTERNAL_SCRIPTS {
                    notes.push(format!(
                        "[skipped script (limit {MAX_EXTERNAL_SCRIPTS} reached): {url}]"
                    ));
                    continue;
                }
                fetched += 1;
                match net::fetch(&url) {
                    Ok(resp) if resp.body.len() > MAX_SCRIPT_BYTES => notes.push(format!(
                        "[skipped large script: {} ({} bytes)]",
                        url,
                        resp.body.len()
                    )),
                    Ok(resp) => classic.push(String::from_utf8_lossy(&resp.body).into_owned()),
                    Err(e) => notes.push(format!("[failed to load script: {url} — {e}]")),
                }
            }
        }
    }

    // ES module graph.
    let (entries, module_sources, mod_notes) = collect_module_graph(&doc, base);
    notes.extend(mod_notes);

    // Note: we previously short-circuited to `(None, …)` when a page had no scripts. That left
    // script-less pages with no live JS runtime, so `console_eval` / WebDriver script execution
    // (and any later runtime DOM query) returned "(no live page)". A real browser always exposes a
    // scriptable document, so we now start a session even with zero author scripts — the cost is one
    // idle isolate, and it makes `Engine::console_eval` work on every loaded HTML page.

    let fetcher: Box<dyn Fn(&str) -> Option<(String, String)> + Send> = Box::new(|u: &str| {
        net::fetch(u).ok().map(|r| {
            (
                String::from_utf8_lossy(&r.body).into_owned(),
                r.content_type,
            )
        })
    });
    let request_fetcher = build_request_fetcher();
    let ws_connector = build_ws_connector();
    let cookie_getter = build_cookie_getter();
    let cookie_setter = build_cookie_setter();
    let (session, snapshot, results) = js::Session::new(
        doc,
        classic,
        entries,
        module_sources,
        base,
        fetcher,
        request_fetcher,
        ws_connector,
        cookie_getter,
        cookie_setter,
        initial_rects,
    );
    for result in results {
        notes.extend(result.console);
        if let Some(err) = result.error {
            notes.push(format!("⚠ {err}"));
        }
    }
    (Some(session), snapshot, notes)
}
