use crate::*;

/// Tags whose subtrees contribute no visible text.
pub(crate) const SKIP_SUBTREE: &[&str] = &["script", "style", "head", "title", "noscript"];

/// Block-ish tags that introduce a line break around their content.
pub(crate) const BLOCK_TAGS: &[&str] = &[
    "p", "div", "h1", "h2", "h3", "h4", "h5", "h6", "li", "br", "section", "article", "header",
    "footer", "ul", "ol", "tr",
];

/// Walk the DOM depth-first and collect visible text, skipping non-rendered subtrees,
/// collapsing ASCII whitespace runs to single spaces, and inserting `\n` around block
/// elements. The result is a reasonable approximation of the page's plain text.
pub fn extract_visible_text(doc: &dom::Document) -> String {
    let mut out = String::new();
    collect_text(doc, doc.root(), &mut out);
    collapse_whitespace(&out)
}

/// Maximum number of external stylesheets fetched per page (including transitively `@import`ed
/// files); the rest are skipped with a note. Sized to accommodate `@import` manifests (a single
/// `<link>` can pull in many component CSS files) while still capping runaway / cyclic imports.
pub(crate) const MAX_EXTERNAL_STYLESHEETS: usize = 100;
/// Maximum number of external scripts fetched per page; the rest are skipped with a note.
pub(crate) const MAX_EXTERNAL_SCRIPTS: usize = 24;
/// Skip fetched script bodies larger than this (mirrors the inline-script cap). Large SPA
/// frameworks ship multi-MB bundles (e.g. youtube's main app bundle is ~10.5 MB), so the cap is
/// generous; V8 parses lazily and the per-run execution budget bounds the time.
pub(crate) const MAX_SCRIPT_BYTES: usize = 32 * 1024 * 1024;

/// One author stylesheet source in document order: either an inline `<style>` body or an
/// external `<link rel=stylesheet href>` whose `href` resolved to an absolute URL.
#[derive(Debug, PartialEq, Eq)]
pub enum StyleSource {
    Inline(String),
    External(String),
}

/// Build the host `request_fetcher` capability passed into the JS runtime: it backs the rewritten
/// JS `fetch()` (arbitrary method + headers + body). Given `(method, resolved_url, body,
/// headers_json)` it parses the headers JSON object, issues the request via [`net::request`], and
/// returns a JSON *envelope* string the JS side parses into a `Response`. Returns `None` on
/// transport error (→ `fetch` rejects with `TypeError`). Runs on the JS worker thread; blocking is
/// fine there.
pub(crate) fn build_request_fetcher(
) -> std::sync::Arc<dyn Fn(&str, &str, &str, &str) -> Option<String> + Send + Sync> {
    std::sync::Arc::new(|method: &str, url: &str, body: &str, headers_json: &str| {
        let headers = parse_headers_json(headers_json);
        let body_opt: Option<&[u8]> = if body.is_empty() {
            None
        } else {
            Some(body.as_bytes())
        };
        let resp = net::request(method, url, body_opt, &headers).ok()?;
        let ok = (200..300).contains(&resp.status);
        let status_text = reason_phrase(resp.status);
        let body_str = String::from_utf8_lossy(&resp.body);
        Some(build_response_envelope(
            ok,
            resp.status,
            status_text,
            &resp.final_url,
            &resp.content_type,
            &body_str,
        ))
    })
}

/// Build the host WebSocket *connector* passed into the JS [`js::Session`]: it backs the real
/// `WebSocket` class. Given `(url, id, evt_tx)` it spawns a dedicated thread running [`net::ws_run`]
/// for the lifetime of that socket and returns the `out` sender the JS side uses to send/close.
/// Returns `Err` only if the thread can't be spawned (in which case the JS object fires
/// onerror/onclose synthetically). Crosses the `js` crate boundary with PRIMITIVE tuple channels
/// only (just like `request_fetcher`), so `js` never depends on `net`.
///
/// Tuple protocol (see [`net::ws_run`]): events `(id, kind, payload)` flow over `evt_tx`; outgoing
/// commands `(kind, payload)` flow over the returned sender.
pub(crate) type WsConnector = std::sync::Arc<
    dyn Fn(
            String,
            u64,
            std::sync::mpsc::Sender<(u64, u8, String)>,
        ) -> Result<std::sync::mpsc::Sender<(u8, String)>, String>
        + Send
        + Sync,
>;

pub(crate) fn build_ws_connector() -> WsConnector {
    std::sync::Arc::new(
        |url: String,
         id: u64,
         evt_tx: std::sync::mpsc::Sender<(u64, u8, String)>|
         -> Result<std::sync::mpsc::Sender<(u8, String)>, String> {
            // The JS side sends/closes through `out_tx`; the worker thread owns `out_rx`.
            let (out_tx, out_rx) = std::sync::mpsc::channel::<(u8, String)>();
            std::thread::Builder::new()
                .name("ws".to_string())
                .spawn(move || {
                    net::ws_run(url, id, evt_tx, out_rx);
                })
                .map_err(|e| format!("could not start WebSocket thread: {e}"))?;
            Ok(out_tx)
        },
    )
}

/// Parse a flat JSON object of `name -> string-value` (the headers JSON `fetch` builds with
/// `JSON.stringify`) into a `Vec<(name, value)>`. Tolerant: returns an empty vec on any parse
/// problem (a malformed header map shouldn't abort the request).
pub(crate) fn parse_headers_json(s: &str) -> Vec<(String, String)> {
    let s = s.trim();
    let inner = match s.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
        Some(i) => i.trim(),
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    let bytes = inner.as_bytes();
    let mut i = 0usize;
    // Parse a JSON string starting at `bytes[i] == '"'`, returning (decoded, next_index).
    fn parse_str(bytes: &[u8], mut i: usize) -> Option<(String, usize)> {
        if bytes.get(i) != Some(&b'"') {
            return None;
        }
        i += 1;
        let mut out = String::new();
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'\\' {
                i += 1;
                match bytes.get(i)? {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'u' => {
                        let hex = std::str::from_utf8(bytes.get(i + 1..i + 5)?).ok()?;
                        let cp = u32::from_str_radix(hex, 16).ok()?;
                        out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                        i += 4;
                    }
                    other => out.push(*other as char),
                }
                i += 1;
            } else if c == b'"' {
                return Some((out, i + 1));
            } else {
                // Copy a UTF-8 byte run up to the next escape/quote.
                let start = i;
                while i < bytes.len() && bytes[i] != b'\\' && bytes[i] != b'"' {
                    i += 1;
                }
                out.push_str(std::str::from_utf8(&bytes[start..i]).ok()?);
            }
        }
        None
    }
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let (key, ni) = match parse_str(bytes, i) {
            Some(r) => r,
            None => break,
        };
        i = ni;
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b':') {
            i += 1;
        }
        let (val, ni) = match parse_str(bytes, i) {
            Some(r) => r,
            None => break,
        };
        i = ni;
        out.push((key, val));
        while i < bytes.len() && (bytes[i] == b',' || (bytes[i] as char).is_whitespace()) {
            i += 1;
        }
    }
    out
}

/// JSON-escape a string into `out` (control chars, quotes, backslash). No surrounding quotes.
/// A `"`-quoted, escaped JSON string literal.
pub(crate) fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    json_escape(s, &mut out);
    out.push('"');
    out
}

pub(crate) fn json_escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

/// Build the JSON response envelope the JS `fetch()` parses into a `Response`.
pub(crate) fn build_response_envelope(
    ok: bool,
    status: u16,
    status_text: &str,
    url: &str,
    content_type: &str,
    body: &str,
) -> String {
    let mut s = String::with_capacity(body.len() + 128);
    s.push_str("{\"ok\":");
    s.push_str(if ok { "true" } else { "false" });
    s.push_str(",\"status\":");
    s.push_str(&status.to_string());
    s.push_str(",\"statusText\":\"");
    json_escape(status_text, &mut s);
    s.push_str("\",\"url\":\"");
    json_escape(url, &mut s);
    s.push_str("\",\"contentType\":\"");
    json_escape(content_type, &mut s);
    s.push_str("\",\"body\":\"");
    json_escape(body, &mut s);
    s.push_str("\"}");
    s
}

/// A minimal reason-phrase for common HTTP status codes (empty when unknown).
pub(crate) fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}

/// Resolve `href` against `base` using the `url` crate, returning an absolute
/// `http(s)`/`file` URL. Returns `None` for empty/fragment-only hrefs and for non-fetchable
/// schemes (`data:`, `javascript:`, `mailto:`, …) or anything that fails to parse/join.
pub fn resolve_url(base: &str, href: &str) -> Option<String> {
    let trimmed = href.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    for bad in ["javascript:", "data:", "mailto:", "tel:", "blob:", "about:"] {
        if lower.starts_with(bad) {
            return None;
        }
    }
    let base = url::Url::parse(base).ok()?;
    let joined = base.join(trimmed).ok()?;
    match joined.scheme() {
        "http" | "https" | "file" => Some(joined.into()),
        _ => None,
    }
}

/// Determine the page's base URL: the response's `final_url`, overridden by the `href` of the
/// first `<base href>` element (resolved against `final_url`) if one is present.
pub fn base_url(doc: &dom::Document, final_url: &str) -> String {
    fn find_base(doc: &dom::Document, id: dom::NodeId) -> Option<String> {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag == "base" {
                if let Some(href) = e.attrs.get("href") {
                    return Some(href.clone());
                }
            }
        }
        for &child in &doc.get(id).children {
            if let Some(h) = find_base(doc, child) {
                return Some(h);
            }
        }
        None
    }
    match find_base(doc, doc.root()) {
        Some(href) => resolve_url(final_url, &href).unwrap_or_else(|| final_url.to_string()),
        None => final_url.to_string(),
    }
}

/// Walk the DOM in document order, classifying each author style contribution as an inline
/// `<style>` body or an external `<link rel=stylesheet href>` (resolved against `base`).
/// Pure: no fetching, so the ordering/classification is unit-testable without network.
/// Concatenate a `<style>`/`<script>` element's text children, stripping an XHTML `<![CDATA[ … ]]>`
/// wrapper (and the legacy `<!-- … -->` comment guard). XHTML files (e.g. WPT's `.xht` reftests)
/// wrap inline CSS/JS in CDATA so the XML parser doesn't treat `&`/`<` as markup; our lenient HTML
/// parser captures `<style>` as raw text, so those markers land literally in the CSS — where they
/// break `@import` extraction (the leading `<![CDATA[` makes the scanner read `@import …;` as a
/// normal rule's prelude and skip it, so the imported sheet is never fetched). Only the surrounding
/// wrapper is removed (markers never appear mid-CSS in practice), leaving the CSS otherwise intact.
pub(crate) fn raw_text_content(doc: &dom::Document, id: dom::NodeId) -> String {
    let mut src = String::new();
    for &child in &doc.get(id).children {
        if let dom::NodeData::Text(t) = &doc.get(child).data {
            src.push_str(t);
        }
    }
    let trimmed = src.trim();
    for (open, close) in [("<![CDATA[", "]]>"), ("<!--", "-->")] {
        if let Some(inner) = trimmed.strip_prefix(open) {
            return inner.strip_suffix(close).unwrap_or(inner).to_string();
        }
    }
    src
}

pub fn collect_style_sources(doc: &dom::Document, base: &str) -> Vec<StyleSource> {
    fn walk(doc: &dom::Document, id: dom::NodeId, base: &str, out: &mut Vec<StyleSource>) {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            match e.tag.as_str() {
                "style" => {
                    out.push(StyleSource::Inline(raw_text_content(doc, id)));
                    return;
                }
                "link" => {
                    let rel = e.attrs.get("rel").map(String::as_str).unwrap_or("");
                    let is_sheet = rel
                        .split_whitespace()
                        .any(|t| t.eq_ignore_ascii_case("stylesheet"));
                    if is_sheet {
                        if let Some(href) = e.attrs.get("href") {
                            if let Some(abs) = resolve_url(base, href) {
                                out.push(StyleSource::External(abs));
                            }
                        }
                    }
                    return;
                }
                _ => {}
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

/// Collect author stylesheets in document order: inline `<style>` bodies parsed directly, and
/// external `<link rel=stylesheet>` sheets fetched (against `base`) then parsed. Returns the
/// ordered sheets plus any console notes (skipped/failed/over-limit). External fetches are
/// sequential. The cascade order UA < these (DOM order) < inline `style=""` is preserved
/// because this list is interleaved by document position.
pub fn collect_stylesheets(doc: &dom::Document, base: &str) -> (Vec<css::Stylesheet>, Vec<String>) {
    let mut sheets = Vec::new();
    let mut console = Vec::new();
    let mut fetched = 0usize;
    // URLs already fetched (across all sources) so a file imported twice isn't refetched, and
    // import cycles terminate.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for source in collect_style_sources(doc, base) {
        match source {
            StyleSource::Inline(src) => {
                // Inline `<style>` may itself `@import` (rare, but cheap): resolve those against
                // the page/base URL, recursively pulling them in BEFORE the inline body's rules.
                process_css_text(
                    &src,
                    base,
                    &mut sheets,
                    &mut console,
                    &mut fetched,
                    &mut seen,
                );
            }
            StyleSource::External(url) => {
                fetch_css(&url, &mut sheets, &mut console, &mut fetched, &mut seen);
            }
        }
    }
    (sheets, console)
}

/// Collect ONLY inline `<style>` stylesheets, parsed directly — NO `<link>`/`@import` network
/// fetches. Used for PARTIAL frames during a streaming load so early paints never block on the
/// network: they show page structure plus inline-CSS styling, and the final frame (built with the
/// full [`collect_stylesheets`]) adds external CSS. Inline `@import`s are intentionally NOT followed
/// here (they'd fetch); the cascade still applies its UA stylesheet on top of these.
pub(crate) fn collect_inline_stylesheets(doc: &dom::Document, base: &str) -> Vec<css::Stylesheet> {
    fn walk(doc: &dom::Document, id: dom::NodeId, base: &str, out: &mut Vec<css::Stylesheet>) {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag == "style" {
                // Inline `<style>` resolves relative `url(...)` against the document base URL.
                out.push(css::parse_with_base(&raw_text_content(doc, id), base));
                return; // a <style>'s text body isn't markup
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

/// Heuristic HTML sniff for responses with an absent/generic `content_type`: true if the parsed
/// document contains any of the structural html/head/body/`<!doctype html>`-derived elements (the
/// lenient parser synthesizes `<html>`/`<head>`/`<body>` for real markup, but a plain-text body
/// parses to bare text under the root with no such elements).
pub(crate) fn document_looks_like_html(doc: &dom::Document) -> bool {
    fn walk(doc: &dom::Document, id: dom::NodeId) -> bool {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            let t = e.tag.as_str();
            if t == "html" || t == "head" || t == "body" || t == "p" || t == "div" {
                return true;
            }
        }
        doc.get(id).children.iter().any(|&c| walk(doc, c))
    }
    walk(doc, doc.root())
}

/// Parse `text` (a CSS body fetched/found at URL `base_url`) and append its stylesheet to
/// `sheets`, but FIRST follow each top-level `@import`: resolve the specifier against `base_url`,
/// recursively fetch+process it, and include its rules before `text`'s own (CSS precedence:
/// imported styles come first / lower precedence, in import order). `fetched`/`seen` track the
/// global file count cap and dedup.
pub(crate) fn process_css_text(
    text: &str,
    base_url: &str,
    sheets: &mut Vec<css::Stylesheet>,
    console: &mut Vec<String>,
    fetched: &mut usize,
    seen: &mut std::collections::HashSet<String>,
) {
    for spec in css::extract_imports(text) {
        match resolve_url(base_url, &spec) {
            Some(abs) => fetch_css(&abs, sheets, console, fetched, seen),
            None => console.push(format!("[skipped @import (unresolvable): {spec}]")),
        }
    }
    sheets.push(css::parse_with_base(text, base_url));
}

/// Fetch the external CSS at absolute URL `url`, then process it (following its own `@import`s).
/// Dedups against `seen` and enforces the [`MAX_EXTERNAL_STYLESHEETS`] fetch cap. A failed fetch
/// is a console note, not a panic.
pub(crate) fn fetch_css(
    url: &str,
    sheets: &mut Vec<css::Stylesheet>,
    console: &mut Vec<String>,
    fetched: &mut usize,
    seen: &mut std::collections::HashSet<String>,
) {
    if !seen.insert(url.to_string()) {
        return; // already fetched (dedup / cycle guard)
    }
    if *fetched >= MAX_EXTERNAL_STYLESHEETS {
        console.push(format!(
            "[skipped stylesheet (limit {MAX_EXTERNAL_STYLESHEETS} reached): {url}]"
        ));
        return;
    }
    *fetched += 1;
    match net::fetch(url) {
        Ok(resp) => {
            let text = String::from_utf8_lossy(&resp.body).into_owned();
            // Resolve this file's own `@import`s relative to the URL it was fetched under.
            process_css_text(&text, url, sheets, console, fetched, seen);
        }
        Err(e) => console.push(format!("[failed to load stylesheet: {url} — {e}]")),
    }
}

/// Walk the DOM in document order collecting `<img>` elements with a resolvable `src`, then
/// fetch + decode each into a [`DecodedImage`] keyed by its DOM node. Caps the number fetched
/// ([`MAX_IMAGES`]) and skips oversized decodes ([`MAX_IMAGE_PIXELS`]). Decode/fetch failures
/// are skipped (with a console note) and never panic. `data:` URLs are decoded inline (base64
/// or percent-encoded); SVG payloads decode but don't raster (`image` has no SVG support).
pub(crate) fn collect_images(
    doc: &dom::Document,
    base: &str,
    console: &mut Vec<String>,
) -> HashMap<dom::NodeId, DecodedImage> {
    // Gather (node, absolute-url) pairs in document order.
    fn walk(
        doc: &dom::Document,
        id: dom::NodeId,
        base: &str,
        out: &mut Vec<(dom::NodeId, String)>,
    ) {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag.eq_ignore_ascii_case("img") {
                if let Some(src) = e.attrs.get("src") {
                    let src = src.trim();
                    // Keep `data:` URLs verbatim (decoded inline below); resolve the rest.
                    if src.starts_with("data:") {
                        out.push((id, src.to_string()));
                    } else if let Some(abs) = resolve_url(base, src) {
                        out.push((id, abs));
                    }
                }
            }
        }
        for &child in &doc.get(id).children {
            walk(doc, child, base, out);
        }
    }
    let mut targets = Vec::new();
    walk(doc, doc.root(), base, &mut targets);
    if targets.len() > MAX_IMAGES {
        for (_, url) in targets.drain(MAX_IMAGES..) {
            console.push(format!(
                "[skipped image (limit {MAX_IMAGES} reached): {url}]"
            ));
        }
    }

    // `data:` images decode inline (no I/O); network images are fetched concurrently across a
    // small pool of scoped threads, since they're independent and order doesn't matter.
    let (data_targets, net_targets): (Vec<_>, Vec<_>) = targets
        .into_iter()
        .partition(|(_, url)| url.starts_with("data:"));

    let mut results: Vec<(dom::NodeId, String, Result<DecodedImage, String>)> = Vec::new();
    for (node, url) in data_targets {
        let r = decode_data_url(&url)
            .ok_or_else(|| "malformed data: URL".to_string())
            .and_then(|b| {
                decode_any_image(&b, "", &url).ok_or_else(|| "decode failed".to_string())
            });
        results.push((node, url, r));
    }

    if !net_targets.is_empty() {
        // Cap concurrency so we don't fire a whole page's images at one origin at once — bursts trip
        // CDN rate limits (e.g. Wikimedia returns 429). At most `MAX_CONCURRENT_IMAGE_FETCHES` are
        // in flight; each worker drains its chunk sequentially.
        let n_threads = net_targets.len().clamp(1, MAX_CONCURRENT_IMAGE_FETCHES);
        let chunks: Vec<Vec<(dom::NodeId, String)>> = {
            let mut cs: Vec<Vec<_>> = (0..n_threads).map(|_| Vec::new()).collect();
            for (i, t) in net_targets.into_iter().enumerate() {
                cs[i % n_threads].push(t);
            }
            cs
        };
        std::thread::scope(|s| {
            let handles: Vec<_> = chunks
                .into_iter()
                .map(|chunk| {
                    s.spawn(move || {
                        chunk
                            .into_iter()
                            .map(|(node, url)| {
                                let r = net::fetch(&url).and_then(|resp| {
                                    decode_any_image(&resp.body, &resp.content_type, &url)
                                        .ok_or_else(|| "decode failed".to_string())
                                });
                                (node, url, r)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            for h in handles {
                results.extend(h.join().unwrap_or_default());
            }
        });
    }

    let mut images = HashMap::new();
    for (node, url, r) in results {
        match r {
            Ok(img) => {
                images.insert(node, img);
            }
            Err(e) => {
                let label = if url.starts_with("data:") {
                    "data: image"
                } else {
                    &url
                };
                console.push(format!("[failed to load image: {label} — {e}]"));
            }
        }
    }
    images
}

/// Decode raster image bytes into straight-alpha RGBA8. Returns `None` on decode failure or if
/// the decoded image would exceed [`MAX_IMAGE_PIXELS`]. Never panics.
pub(crate) fn decode_image(bytes: &[u8]) -> Option<DecodedImage> {
    // The `image` crate has no JPEG XL decoder, so route `.jxl` bytes to `jxl-oxide` (pure Rust).
    if is_jxl(bytes) {
        return decode_jxl(bytes);
    }
    let dynimg = image::load_from_memory(bytes).ok()?;
    let w = dynimg.width();
    let h = dynimg.height();
    if (w as u64) * (h as u64) > MAX_IMAGE_PIXELS {
        return None;
    }
    let rgba = dynimg.to_rgba8();
    Some(DecodedImage {
        rgba: rgba.into_raw(),
        w,
        h,
    })
}

/// Max favicon download size — favicons are tiny; this caps abuse without rejecting real icons.
const MAX_FAVICON_BYTES: usize = 2 * 1024 * 1024;
/// Pixel size favicons are decoded/rasterized to for the tab + address-bar slot (covers 2× of the
/// ~16pt UI icon; the shell downsamples to fit).
const FAVICON_PX: u32 = 32;

/// Whether `url`'s path has a known image extension (used to render direct image navigations even
/// when the server sends a generic content-type).
pub(crate) fn url_has_image_extension(url: &str) -> bool {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    [
        ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".ico", ".svg", ".avif", ".jxl", ".tif",
        ".tiff",
    ]
    .iter()
    .any(|ext| path.ends_with(ext))
}

/// A minimal generated document that displays `url` as a centered image on a neutral backdrop —
/// the "image viewer" real browsers show when you navigate straight to an image file.
pub(crate) fn image_viewer_html(url: &str) -> String {
    // Escape the few characters that would break the `src="..."` attribute.
    let safe = url.replace('&', "&amp;").replace('"', "&quot;");
    format!(
        "<!DOCTYPE html><html><head><meta name=\"color-scheme\" content=\"light dark\"></head>\
         <body style=\"margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;background:#2b2b2b\">\
         <img src=\"{safe}\" style=\"max-width:100%;height:auto\"></body></html>"
    )
}

/// Resolve the page's favicon URL: the first `<link rel~="icon">` href (or a `data:` icon),
/// resolved against `base`; otherwise the origin's `/favicon.ico`. `None` when no http(s) origin can
/// be derived (e.g. a `file://` page with no explicit icon link).
pub(crate) fn resolve_favicon_url(doc: &dom::Document, base: &str) -> Option<String> {
    fn walk(doc: &dom::Document, id: dom::NodeId, base: &str) -> Option<String> {
        if let dom::NodeData::Element(e) = &doc.get(id).data {
            if e.tag.eq_ignore_ascii_case("link") {
                let rel = e.attrs.get("rel").map(String::as_str).unwrap_or("");
                // `rel="icon"` / `rel="shortcut icon"` (token match; excludes `apple-touch-icon`).
                let is_icon = rel
                    .split_whitespace()
                    .any(|t| t.eq_ignore_ascii_case("icon"));
                if is_icon {
                    if let Some(href) = e.attrs.get("href") {
                        let h = href.trim();
                        if h.starts_with("data:") {
                            return Some(h.to_string()); // inline icon; decoded directly later
                        }
                        if let Some(abs) = resolve_url(base, h) {
                            return Some(abs);
                        }
                    }
                }
            }
        }
        for &child in &doc.get(id).children {
            if let Some(u) = walk(doc, child, base) {
                return Some(u);
            }
        }
        None
    }
    if let Some(u) = walk(doc, doc.root(), base) {
        return Some(u);
    }
    // Fallback to <origin>/favicon.ico, but only for http(s) pages.
    let parsed = url::Url::parse(base).ok()?;
    match parsed.scheme() {
        "http" | "https" => parsed.join("/favicon.ico").ok().map(Into::into),
        _ => None,
    }
}

/// Fetch + decode the favicon at `url` into RGBA8. Handles raster formats (ICO/PNG/JPEG/GIF/WebP)
/// via the image decoder and SVG via the engine's own rasterizer; `data:` icons decode inline.
/// Best-effort: any failure (network, unsupported format, decode error) returns `None`.
pub(crate) fn fetch_favicon(url: &str, font: Option<&SystemFont>) -> Option<DecodedImage> {
    // Bytes + a lowercased content-type/MIME hint.
    let (bytes, ctype) = if url.starts_with("data:") {
        let mime = url
            .get("data:".len()..)?
            .split([';', ','])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        (decode_data_url(url)?, mime)
    } else {
        let resp = net::fetch(url).ok()?;
        if resp.body.len() > MAX_FAVICON_BYTES {
            return None;
        }
        (resp.body, resp.content_type.to_ascii_lowercase())
    };
    if bytes.is_empty() {
        return None;
    }
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if is_svg_source(&ctype, &path, &bytes) {
        decode_svg_sized(&bytes, FAVICON_PX, FAVICON_PX, font)
    } else {
        decode_image(&bytes)
    }
}

/// Decode image bytes whose format may be SVG (which the `image` crate can't handle) — used by
/// `<img>` collection. Raster formats go through [`decode_image`]; SVG is parsed and rasterized at
/// its intrinsic size via the engine's own renderer (text in SVG images isn't drawn — no font is
/// threaded into the off-main-thread image fetch). `ctype` is the response content-type (may be
/// empty for `data:` URLs), `url` the source URL (for the `.svg` extension hint).
pub(crate) fn decode_any_image(bytes: &[u8], ctype: &str, url: &str) -> Option<DecodedImage> {
    let ct = ctype.to_ascii_lowercase();
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if is_svg_source(&ct, &path, bytes) {
        decode_svg_image(bytes)
    } else {
        decode_image(bytes)
    }
}

/// Whether a fetched resource is SVG, by content-type, `.svg` extension, or a markup sniff.
fn is_svg_source(ctype: &str, path: &str, bytes: &[u8]) -> bool {
    ctype.contains("svg") || path.ends_with(".svg") || bytes_look_like_svg(bytes)
}

/// Heuristic sniff for SVG markup (covers responses served with a wrong/missing content-type).
fn bytes_look_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(512)];
    let s = String::from_utf8_lossy(head);
    let s = s.trim_start();
    s.starts_with("<svg")
        || ((s.starts_with("<?xml") || s.starts_with("<!--")) && s.contains("<svg"))
}

/// The node id of the first `<svg>` element anywhere in `doc`.
fn find_svg_root(doc: &dom::Document, id: dom::NodeId) -> Option<dom::NodeId> {
    if let dom::NodeData::Element(e) = &doc.get(id).data {
        if e.tag.eq_ignore_ascii_case("svg") {
            return Some(id);
        }
    }
    for &c in &doc.get(id).children {
        if let Some(s) = find_svg_root(doc, c) {
            return Some(s);
        }
    }
    None
}

/// Parse standalone SVG markup and rasterize its first `<svg>` element to a `w`×`h` bitmap.
fn decode_svg_sized(
    bytes: &[u8],
    w: u32,
    h: u32,
    font: Option<&SystemFont>,
) -> Option<DecodedImage> {
    let markup = String::from_utf8_lossy(bytes);
    let doc = html::parse(&markup);
    let svg_id = find_svg_root(&doc, doc.root())?;
    Some(crate::svg::rasterize_svg(&doc, svg_id, w, h, font))
}

/// Parse standalone SVG markup and rasterize it at its intrinsic size (from width/height/viewBox),
/// clamped so a missing/huge size can't blow up memory. For `<img src="*.svg">`.
fn decode_svg_image(bytes: &[u8]) -> Option<DecodedImage> {
    let markup = String::from_utf8_lossy(bytes);
    let doc = html::parse(&markup);
    let svg_id = find_svg_root(&doc, doc.root())?;
    let (iw, ih) = match &doc.get(svg_id).data {
        dom::NodeData::Element(e) => crate::svg::intrinsic_size(e),
        _ => return None,
    };
    let w = (iw.round() as u32).clamp(1, 1024);
    let h = (ih.round() as u32).clamp(1, 1024);
    Some(crate::svg::rasterize_svg(&doc, svg_id, w, h, None))
}

/// Whether `bytes` look like a JPEG XL stream: either the raw codestream marker (`FF 0A`) or the
/// ISOBMFF container's signature box (`00000000C 'JXL ' 0D 0A 87 0A`).
pub(crate) fn is_jxl(bytes: &[u8]) -> bool {
    const CONTAINER: [u8; 12] = [
        0x00, 0x00, 0x00, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A,
    ];
    bytes.starts_with(&[0xFF, 0x0A]) || bytes.starts_with(&CONTAINER)
}

/// Decode a JPEG XL image to straight-alpha RGBA8 via `jxl-oxide`. The decoder yields interleaved
/// `f32` samples in `[0, 1]` with 1 (gray), 2 (gray+alpha), 3 (RGB) or 4 (RGBA) channels; we expand
/// each to RGBA8. Returns `None` on a decode failure or if the image exceeds [`MAX_IMAGE_PIXELS`].
pub(crate) fn decode_jxl(bytes: &[u8]) -> Option<DecodedImage> {
    let image = jxl_oxide::JxlImage::builder().read(bytes).ok()?;
    let w = image.width();
    let h = image.height();
    if (w as u64) * (h as u64) > MAX_IMAGE_PIXELS {
        return None;
    }
    let render = image.render_frame(0).ok()?;
    let fb = render.image_all_channels();
    let channels = fb.channels();
    if channels == 0 {
        return None;
    }
    let buf = fb.buf();
    let px = (w as usize).checked_mul(h as usize)?;
    if buf.len() < px * channels {
        return None;
    }
    let to_u8 = |f: f32| (f.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    let mut rgba = Vec::with_capacity(px * 4);
    for i in 0..px {
        let base = i * channels;
        let (r, g, b, a) = match channels {
            1 => {
                let v = to_u8(buf[base]);
                (v, v, v, 255)
            }
            2 => {
                let v = to_u8(buf[base]);
                (v, v, v, to_u8(buf[base + 1]))
            }
            3 => (
                to_u8(buf[base]),
                to_u8(buf[base + 1]),
                to_u8(buf[base + 2]),
                255,
            ),
            _ => (
                to_u8(buf[base]),
                to_u8(buf[base + 1]),
                to_u8(buf[base + 2]),
                to_u8(buf[base + 3]),
            ),
        };
        rgba.extend_from_slice(&[r, g, b, a]);
    }
    Some(DecodedImage { rgba, w, h })
}

/// Decode a `data:[<mediatype>][;base64],<data>` URL into its raw bytes. Returns `None` if it
/// isn't a well-formed data URL. (SVG data URLs decode fine here but won't raster — `image`
/// has no SVG support — and are dropped at the `decode_image` step.)
pub(crate) fn decode_data_url(url: &str) -> Option<Vec<u8>> {
    let rest = url.strip_prefix("data:")?;
    let comma = rest.find(',')?;
    let meta = &rest[..comma];
    let payload = &rest[comma + 1..];
    if meta.split(';').any(|t| t.eq_ignore_ascii_case("base64")) {
        base64_decode(payload)
    } else {
        Some(percent_decode(payload))
    }
}

/// Minimal standard/URL-safe base64 decoder (ignores padding/whitespace). No external dep.
pub(crate) fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        })
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut buf, mut bits) = (0u32, 0u32);
    for &c in s.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        buf = (buf << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// Percent-decode bytes (`%HH`), passing other bytes through.
pub(crate) fn percent_decode(s: &str) -> Vec<u8> {
    fn hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Draw a single run at `(x, baseline)`. If `bold`, approximate bold by drawing each glyph
/// twice with a 1px horizontal offset ("faux bold"). `letter_spacing` px is added per character.
/// Returns the final pen x (end of the run), used to size text-decoration underlines.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_run(
    fb: &mut Framebuffer,
    font: &dyn GlyphRasterizer,
    text: &str,
    x: f32,
    baseline_y: f32,
    px: f32,
    color: Color,
    bold: bool,
    letter_spacing: f32,
) -> f32 {
    let end = draw_text_spaced(fb, font, text, x, baseline_y, px, color, letter_spacing);
    if bold {
        draw_text_spaced(
            fb,
            font,
            text,
            x + 1.0,
            baseline_y,
            px,
            color,
            letter_spacing,
        );
    }
    end
}
