//! The W3C WebDriver server: a tiny blocking HTTP/1.1 server plus the command dispatcher.
//!
//! Concurrency model: each `Engine` (and its V8 isolate) is single-threaded, so we serialize all
//! work behind one mutex-guarded [`Sessions`] map and process one HTTP request per connection. This
//! is plenty for a single WebDriver client (`wptrunner` drives one session at a time).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;

use crate::json::{obj, parse, Json};

/// The W3C web element identifier key. Every element reference is a single-key object using it.
const ELEMENT_KEY: &str = "element-6066-11e4-a52e-4f735466cecf";

/// A parked (non-current) window: its engine and last-loaded URL, set aside while another window is
/// current. Switching windows swaps one of these into the [`Session`]'s current `engine`/`url`.
struct ParkedWindow {
    engine: engine::Engine,
    url: String,
}

/// A driven browser session. The engine has one V8 isolate per *window*, and WebDriver is
/// inherently multi-window (wptrunner opens a separate window per test), so a session holds the
/// current window inline (`engine`/`url` — kept as fields so every command operates on the current
/// window unchanged) plus any number of `parked` background windows. `order` tracks all live window
/// handles in creation order, for `Get Window Handles` and for promoting a survivor on close.
struct Session {
    engine: engine::Engine,
    url: String,
    width: u32,
    height: u32,
    scale: f32,
    /// Handle of the current window (the one `engine`/`url` belong to).
    handle: String,
    /// Background windows, keyed by handle.
    parked: HashMap<String, ParkedWindow>,
    /// All live window handles (current + parked) in creation order.
    order: Vec<String>,
    /// Next window-handle ordinal.
    next_window: u64,
    /// Script timeout in ms, from `Set Timeouts` (default 30s, per the WebDriver default).
    script_timeout_ms: u64,
}

impl Session {
    /// Create a fresh background window at `about:blank` and return its handle. Does not switch to
    /// it (the W3C `New Window` command leaves the current window unchanged).
    fn new_window(&mut self) -> String {
        let handle = format!("window-{}", self.next_window);
        self.next_window += 1;
        let mut engine = engine::Engine::new();
        engine.set_viewport(self.width, self.height, self.scale);
        engine.load_url("about:blank");
        for _ in 0..5 {
            engine.tick();
        }
        self.parked.insert(
            handle.clone(),
            ParkedWindow {
                engine,
                url: String::new(),
            },
        );
        self.order.push(handle.clone());
        handle
    }

    /// Make `target` the current window, parking the previously-current one. `Err` if no such window.
    fn switch_to(&mut self, target: &str) -> Result<(), WdError> {
        if self.handle == target {
            return Ok(());
        }
        let park = self
            .parked
            .remove(target)
            .ok_or_else(WdError::no_such_window)?;
        let prev_engine = std::mem::replace(&mut self.engine, park.engine);
        let prev_url = std::mem::replace(&mut self.url, park.url);
        self.parked.insert(
            std::mem::replace(&mut self.handle, target.to_string()),
            ParkedWindow {
                engine: prev_engine,
                url: prev_url,
            },
        );
        Ok(())
    }

    /// Close the current window and switch to a surviving window (the most recently created).
    /// Returns the remaining handles. If it was the last window, the session is left empty.
    fn close_current(&mut self) -> Vec<String> {
        self.order.retain(|h| h != &self.handle);
        if let Some(next) = self.order.last().cloned() {
            if let Some(park) = self.parked.remove(&next) {
                self.engine = park.engine; // drops the closed window's engine
                self.url = park.url;
                self.handle = next;
            }
        }
        self.order.clone()
    }
}

/// All live sessions, keyed by session id. Guarded by a mutex so the (single-threaded) engines are
/// only ever touched by one request at a time.
#[derive(Default)]
struct Sessions {
    map: HashMap<String, Session>,
}

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// A WebDriver protocol error: maps to an HTTP status + a JSON error body.
struct WdError {
    status: u16,
    code: &'static str,
    message: String,
}

impl WdError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        WdError {
            status,
            code,
            message: message.into(),
        }
    }
    fn invalid_session(id: &str) -> Self {
        WdError::new(
            404,
            "invalid session id",
            format!("No session with id {id}"),
        )
    }
    fn no_such_element() -> Self {
        WdError::new(404, "no such element", "Element not found")
    }
    fn no_such_window() -> Self {
        WdError::new(404, "no such window", "No window with that handle")
    }
    fn invalid_argument(msg: impl Into<String>) -> Self {
        WdError::new(400, "invalid argument", msg)
    }
    fn javascript_error(msg: impl Into<String>) -> Self {
        WdError::new(500, "javascript error", msg)
    }
    fn unknown_command() -> Self {
        WdError::new(404, "unknown command", "Unknown command")
    }
    fn script_timeout() -> Self {
        WdError::new(500, "script timeout", "Script timed out")
    }
}

type WdResult = Result<Json, WdError>;

/// Run the WebDriver server, blocking forever, listening on `127.0.0.1:port`.
pub fn run(port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let actual = listener.local_addr()?.port();
    eprintln!("webdriver listening on http://127.0.0.1:{actual}");
    serve(listener)
}

/// Serve on an already-bound listener (used by tests with an ephemeral port).
///
/// Single-threaded by design: an [`engine::Engine`] owns a V8 isolate that is not `Send`, so every
/// session must live on one thread. WebDriver clients issue commands sequentially, so we handle one
/// connection at a time. The [`Mutex`] is kept for interior-mutability ergonomics.
pub fn serve(listener: TcpListener) -> std::io::Result<()> {
    let sessions = Mutex::new(Sessions::default());
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let _ = handle_connection(stream, &sessions);
    }
    Ok(())
}

/// Read and parse one HTTP request, dispatch it, and write the JSON response.
fn handle_connection(mut stream: TcpStream, sessions: &Mutex<Sessions>) -> std::io::Result<()> {
    let (method, path, body) = match read_request(&mut stream)? {
        Some(req) => req,
        None => return Ok(()),
    };

    let result = dispatch(&method, &path, &body, sessions);
    match result {
        Ok(value) => {
            let payload = obj(vec![("value", value)]).to_string();
            write_response(&mut stream, 200, "OK", &payload)
        }
        Err(e) => {
            let payload = obj(vec![(
                "value",
                obj(vec![
                    ("error", Json::Str(e.code.to_string())),
                    ("message", Json::Str(e.message)),
                    ("stacktrace", Json::Str(String::new())),
                ]),
            )])
            .to_string();
            let reason = match e.status {
                400 => "Bad Request",
                404 => "Not Found",
                500 => "Internal Server Error",
                _ => "Error",
            };
            write_response(&mut stream, e.status, reason, &payload)
        }
    }
}

/// Read an HTTP request: the request line, headers, and (Content-Length-delimited) body.
fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<(String, String, String)>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    // Read until we have the full headers (\r\n\r\n).
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 64 * 1024 * 1024 {
            return Ok(None);
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let mut content_length = 0usize;
    for line in lines {
        if let Some((_, v)) = line.split_once(':') {
            if line.to_ascii_lowercase().starts_with("content-length:") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }

    let body_start = header_end + 4;
    let mut body = buf[body_start..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);
    let body = String::from_utf8_lossy(&body).to_string();
    Ok(Some((method, path, body)))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Write an HTTP/1.1 response with a JSON body (`Connection: close`).
fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-cache\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// Route a request to its command handler. Path segments after `/session/{id}/` select the command.
fn dispatch(method: &str, path: &str, body: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let segs: Vec<&str> = path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    match (method, segs.as_slice()) {
        ("GET", ["status"]) => Ok(obj(vec![
            ("ready", Json::Bool(true)),
            ("message", Json::Str("ok".to_string())),
        ])),
        ("POST", ["session"]) => new_session(body, sessions),
        ("DELETE", ["session", id]) => delete_session(id, sessions),

        ("POST", ["session", id, "url"]) => navigate(id, body, sessions),
        ("GET", ["session", id, "url"]) => get_url(id, sessions),
        ("GET", ["session", id, "title"]) => get_title(id, sessions),
        ("GET", ["session", id, "source"]) => get_source(id, sessions),
        ("POST", ["session", id, "refresh"]) => refresh(id, sessions),
        ("POST", ["session", _id, "back"]) => Ok(Json::Null), // stub: no history wired
        ("POST", ["session", id, "forward"]) => {
            // stub: no history wired — validate session so clients get a sane error.
            with_session(id, sessions, |_| Ok(Json::Null))
        }

        ("POST", ["session", id, "execute", "sync"]) => execute_sync(id, body, sessions),
        ("POST", ["session", id, "execute", "async"]) => execute_async(id, body, sessions),

        ("POST", ["session", id, "element"]) => find_element(id, body, sessions),
        ("POST", ["session", id, "elements"]) => find_elements(id, body, sessions),
        ("GET", ["session", id, "element", eid, "text"]) => element_text(id, eid, sessions),
        ("GET", ["session", id, "element", eid, "attribute", name]) => {
            element_attribute(id, eid, name, sessions)
        }
        ("GET", ["session", id, "element", eid, "property", name]) => {
            element_property(id, eid, name, sessions)
        }
        ("GET", ["session", id, "element", eid, "css", prop]) => {
            element_css(id, eid, prop, sessions)
        }
        ("GET", ["session", id, "element", eid, "name"]) => element_name(id, eid, sessions),
        ("GET", ["session", id, "element", eid, "rect"]) => element_rect(id, eid, sessions),
        ("POST", ["session", id, "element", eid, "click"]) => element_click(id, eid, sessions),
        ("POST", ["session", id, "element", eid, "value"]) => {
            element_value(id, eid, body, sessions)
        }
        ("GET", ["session", id, "element", eid, "screenshot"]) => {
            element_screenshot(id, eid, sessions)
        }

        ("GET", ["session", id, "screenshot"]) => screenshot(id, sessions),

        ("GET", ["session", id, "window", "rect"]) => window_rect(id, sessions),
        ("POST", ["session", id, "window", "rect"]) => set_window_rect(id, body, sessions),
        ("GET", ["session", id, "window", "handles"]) => window_handles(id, sessions),
        ("POST", ["session", id, "window", "new"]) => new_window_cmd(id, body, sessions),
        ("GET", ["session", id, "window"]) => get_window_handle(id, sessions),
        ("POST", ["session", id, "window"]) => switch_window(id, body, sessions),
        ("DELETE", ["session", id, "window"]) => close_window(id, sessions),

        ("POST", ["session", id, "timeouts"]) => set_timeouts(id, body, sessions),
        ("GET", ["session", id, "timeouts"]) => get_timeouts(id, sessions),

        // Actions: `release` (DELETE) runs before every test even when it uses no testdriver input,
        // so it must succeed (and resets the input state — a no-op for us). `perform` (POST) replays
        // pointer input as synthetic mouse events so testdriver click/hover tests run for real.
        ("POST", ["session", id, "actions"]) => perform_actions(id, body, sessions),
        ("DELETE", ["session", id, "actions"]) => with_session(id, sessions, |_| Ok(Json::Null)),

        // Frame switching is not modeled yet; accept and stay on the top-level context so tests that
        // only touch the top document keep working (frame-targeted ones will misbehave, not 404).
        ("POST", ["session", id, "frame"]) => with_session(id, sessions, |_| Ok(Json::Null)),
        ("POST", ["session", id, "frame", "parent"]) => {
            with_session(id, sessions, |_| Ok(Json::Null))
        }

        _ => Err(WdError::unknown_command()),
    }
}

/// Run `f` with the named session locked, mapping a missing id to `invalid session id`.
fn with_session<T>(
    id: &str,
    sessions: &Mutex<Sessions>,
    f: impl FnOnce(&mut Session) -> Result<T, WdError>,
) -> Result<T, WdError> {
    let mut guard = sessions.lock().unwrap();
    let session = guard
        .map
        .get_mut(id)
        .ok_or_else(|| WdError::invalid_session(id))?;
    f(session)
}

// ---------------------------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------------------------

fn new_session(body: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let parsed = parse(body).unwrap_or(Json::Null);
    // Capabilities: accept anything. Pull a window size if the client offered one.
    let caps = parsed.get("capabilities");
    let always = caps.and_then(|c| c.get("alwaysMatch"));
    let first = caps
        .and_then(|c| c.get("firstMatch"))
        .and_then(|c| c.as_array())
        .and_then(|a| a.first());

    // Look for a requested window size under a few common capability shapes.
    let (mut width, mut height) = (800u32, 600u32);
    for src in [always, first].into_iter().flatten() {
        if let Some(w) = window_size_from_caps(src) {
            width = w.0;
            height = w.1;
        }
    }

    let mut engine = engine::Engine::new();
    let scale = 1.0f32;
    engine.set_viewport(width, height, scale);
    // Start on `about:blank` (a real browser's initial document) so script execution / find work
    // before the first navigation.
    engine.load_url("about:blank");
    for _ in 0..5 {
        engine.tick();
    }

    let id = format!(
        "browser-wd-{:08}",
        SESSION_COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    let handle = "window-0".to_string();
    let session = Session {
        engine,
        url: String::new(),
        width,
        height,
        scale,
        handle: handle.clone(),
        parked: HashMap::new(),
        order: vec![handle],
        next_window: 1,
        script_timeout_ms: 30_000,
    };
    sessions.lock().unwrap().map.insert(id.clone(), session);

    let returned_caps = obj(vec![
        ("browserName", Json::Str("from-scratch-browser".to_string())),
        ("browserVersion", Json::Str("0.1.0".to_string())),
        ("platformName", Json::Str(std::env::consts::OS.to_string())),
        ("acceptInsecureCerts", Json::Bool(true)),
        ("setWindowRect", Json::Bool(true)),
        ("proxy", obj(vec![])),
    ]);

    Ok(obj(vec![
        ("sessionId", Json::Str(id)),
        ("capabilities", returned_caps),
    ]))
}

/// Extract a `{width,height}` from a capabilities object, accepting a couple of common shapes.
fn window_size_from_caps(caps: &Json) -> Option<(u32, u32)> {
    // `goog:chromeOptions.windowSize` style "WxH", or an explicit window rect.
    if let Some(rect) = caps.get("windowRect").or_else(|| caps.get("window")) {
        let w = rect.get("width").and_then(|v| v.as_f64());
        let h = rect.get("height").and_then(|v| v.as_f64());
        if let (Some(w), Some(h)) = (w, h) {
            return Some((w as u32, h as u32));
        }
    }
    None
}

fn delete_session(id: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let mut guard = sessions.lock().unwrap();
    // Idempotent: deleting an unknown session is not an error per spec.
    guard.map.remove(id);
    Ok(Json::Null)
}

// ---------------------------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------------------------

fn navigate(id: &str, body: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let parsed = parse(body).unwrap_or(Json::Null);
    let url = parsed
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| WdError::invalid_argument("missing url"))?
        .to_string();

    with_session(id, sessions, |s| {
        s.engine.load_url(&url);
        s.url = url.clone();
        // Tick the event loop until document.readyState === "complete" or a timeout.
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            for _ in 0..5 {
                s.engine.tick();
            }
            let ready = s.engine.console_eval("document.readyState");
            if ready == "complete" {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // A few extra ticks for deferred scripts/microtasks.
        for _ in 0..10 {
            s.engine.tick();
        }
        Ok(Json::Null)
    })
}

fn get_url(id: &str, sessions: &Mutex<Sessions>) -> WdResult {
    with_session(id, sessions, |s| {
        // Prefer the live document URL; fall back to the last requested URL.
        let live = s.engine.console_eval("document.URL || location.href");
        let url = if live.is_empty() || live == "undefined" || live == "(no live page)" {
            s.url.clone()
        } else {
            live
        };
        Ok(Json::Str(url))
    })
}

fn get_title(id: &str, sessions: &Mutex<Sessions>) -> WdResult {
    with_session(id, sessions, |s| {
        Ok(Json::Str(s.engine.title().unwrap_or_default()))
    })
}

fn get_source(id: &str, sessions: &Mutex<Sessions>) -> WdResult {
    with_session(id, sessions, |s| {
        let html = s.engine.console_eval("document.documentElement.outerHTML");
        Ok(Json::Str(html))
    })
}

fn refresh(id: &str, sessions: &Mutex<Sessions>) -> WdResult {
    with_session(id, sessions, |s| {
        let url = s.url.clone();
        if !url.is_empty() {
            s.engine.load_url(&url);
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(20) {
                for _ in 0..5 {
                    s.engine.tick();
                }
                if s.engine.console_eval("document.readyState") == "complete" {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        Ok(Json::Null)
    })
}

// ---------------------------------------------------------------------------------------------
// Script execution
// ---------------------------------------------------------------------------------------------

/// Build a JS array literal of the (already JSON) argument values, with WebDriver element
/// references rewritten to `window.__wd_elements[<handle>]` so the live node is passed in.
fn build_args_js(args: &[Json]) -> String {
    let mut out = String::from("[");
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&arg_to_js(a));
    }
    out.push(']');
    out
}

/// Translate one argument value to a JS expression. Element references become a live-node lookup;
/// everything else is its JSON form (valid JS).
fn arg_to_js(a: &Json) -> String {
    if let Json::Obj(o) = a {
        if o.len() == 1 {
            if let Some(Json::Str(handle)) = o.get(ELEMENT_KEY) {
                if handle.chars().all(|c| c.is_ascii_digit()) {
                    return format!("(window.__wd_elements||[])[{handle}]");
                }
            }
        }
    }
    match a {
        Json::Arr(arr) => {
            let mut s = String::from("[");
            for (i, v) in arr.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&arg_to_js(v));
            }
            s.push(']');
            s
        }
        Json::Obj(o) => {
            let mut s = String::from("{");
            for (i, (k, v)) in o.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&Json::Str(k.clone()).to_string());
                s.push(':');
                s.push_str(&arg_to_js(v));
            }
            s.push('}');
            s
        }
        other => other.to_string(),
    }
}

/// JS preamble that defines `__wd_serialize`: turns a return value into a JSON string, converting
/// DOM nodes into WebDriver element references by registering them in `window.__wd_elements`.
const SERIALIZE_PREAMBLE: &str = r#"
window.__wd_elements = window.__wd_elements || [];
window.__wd_register = function(node){
  var arr = window.__wd_elements;
  for (var i = 0; i < arr.length; i++) { if (arr[i] === node) return i; }
  arr.push(node); return arr.length - 1;
};
window.__wd_serialize = function(v){
  function conv(x){
    if (x === null || x === undefined) return null;
    if (typeof x === "number" || typeof x === "boolean" || typeof x === "string") return x;
    if (typeof x === "function") return null;
    if (typeof Node !== "undefined" && x instanceof Node && x.nodeType === 1) {
      var ref = {}; ref["element-6066-11e4-a52e-4f735466cecf"] = String(window.__wd_register(x)); return ref;
    }
    if (Array.isArray(x) || (typeof x.length === "number" && typeof x !== "string" && x.item)) {
      var out = []; for (var i = 0; i < x.length; i++) out.push(conv(x[i])); return out;
    }
    if (typeof x === "object") {
      var o = {}; for (var k in x) { try { o[k] = conv(x[k]); } catch(e){} } return o;
    }
    return null;
  }
  try { return JSON.stringify(conv(v)); } catch(e) { return JSON.stringify(null); }
};
"#;

fn execute_sync(id: &str, body: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let parsed = parse(body).unwrap_or(Json::Null);
    let script = parsed
        .get("script")
        .and_then(|v| v.as_str())
        .ok_or_else(|| WdError::invalid_argument("missing script"))?
        .to_string();
    let args = parsed
        .get("args")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let args_js = build_args_js(&args);

    with_session(id, sessions, |s| {
        // Run the user script as a function body, serialize the result to JSON.
        let wrapped = format!(
            "{SERIALIZE_PREAMBLE}\n(function(){{ try {{ var __r = (function(){{ {script} \n}}).apply(null, {args_js}); return window.__wd_serialize(__r); }} catch(e) {{ return '__WD_ERR__' + (e && e.message ? e.message : String(e)); }} }})()"
        );
        let raw = s.engine.console_eval(&wrapped);
        // A synchronous script's return value is captured above, but it may have queued tasks that a
        // live event loop would then run — most importantly `window.postMessage`, which delivers on a
        // task. WPT's testdriver delivers action/test-completion results by having wptrunner
        // `execute_script("window.postMessage(...)")`; without pumping the loop here that message
        // never reaches the page's `message` listener and `test_driver` promises hang. Tick a few
        // times so those tasks settle before we return.
        for _ in 0..5 {
            s.engine.tick();
        }
        decode_eval_result(&raw)
    })
}

fn execute_async(id: &str, body: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let parsed = parse(body).unwrap_or(Json::Null);
    let script = parsed
        .get("script")
        .and_then(|v| v.as_str())
        .ok_or_else(|| WdError::invalid_argument("missing script"))?
        .to_string();
    let mut args = parsed
        .get("args")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    // The async script's last argument is the completion callback we inject.
    // Append a placeholder we replace with our resolver below.
    args.push(Json::Null);
    let head_args: Vec<Json> = args[..args.len() - 1].to_vec();
    let head_args_js = build_args_js(&head_args);

    with_session(id, sessions, |s| {
        // Install the result holders + the resolver callback, then run the script with it appended.
        let setup = format!(
            r#"{SERIALIZE_PREAMBLE}
window.__wd_async_done = 0; window.__wd_async_result = null; window.__wd_async_err = null;
(function(){{
  var __cb = function(v){{ try {{ window.__wd_async_result = window.__wd_serialize(v); }} catch(e){{ window.__wd_async_result = JSON.stringify(null); }} window.__wd_async_done = 1; }};
  var __args = {head_args_js}; __args.push(__cb);
  try {{ (function(){{ {script}
  }}).apply(null, __args); }} catch(e){{ window.__wd_async_err = (e && e.message ? e.message : String(e)); window.__wd_async_done = 1; }}
}})();
"#
        );
        let _ = s.engine.console_eval(&setup);

        // Tick the event loop until the callback fires or we hit the session's script timeout.
        let timeout = Duration::from_millis(s.script_timeout_ms);
        let start = Instant::now();
        loop {
            if s.engine.console_eval("window.__wd_async_done || 0") == "1" {
                break;
            }
            if start.elapsed() > timeout {
                return Err(WdError::script_timeout());
            }
            for _ in 0..5 {
                s.engine.tick();
            }
            std::thread::sleep(Duration::from_millis(3));
        }

        let err = s.engine.console_eval("window.__wd_async_err");
        if err != "null" && !err.is_empty() && err != "undefined" {
            return Err(WdError::javascript_error(err));
        }
        let raw = s.engine.console_eval("window.__wd_async_result");
        // `raw` is already a JSON string produced by __wd_serialize.
        parse(&raw).ok_or_else(|| WdError::javascript_error("bad async result"))
    })
}

/// Decode the string `console_eval` returned for an execute wrapper: either our `__WD_ERR__`
/// sentinel (→ javascript error) or a JSON string of the result.
fn decode_eval_result(raw: &str) -> WdResult {
    if let Some(msg) = raw.strip_prefix("__WD_ERR__") {
        return Err(WdError::javascript_error(msg.to_string()));
    }
    parse(raw).ok_or_else(|| WdError::javascript_error(format!("could not parse result: {raw}")))
}

// ---------------------------------------------------------------------------------------------
// Elements
// ---------------------------------------------------------------------------------------------

/// Convert a `using` strategy + value into a CSS selector string (best-effort for non-css ones).
fn selector_for(using: &str, value: &str) -> Result<String, WdError> {
    match using {
        "css selector" => Ok(value.to_string()),
        "tag name" => Ok(value.to_string()),
        "id" => Ok(format!("#{value}")),
        "class name" => Ok(format!(".{value}")),
        // link text strategies are handled separately (need text matching), not via selector.
        "link text" | "partial link text" | "xpath" => Err(WdError::invalid_argument(format!(
            "unsupported locator strategy handled elsewhere: {using}"
        ))),
        other => Err(WdError::invalid_argument(format!(
            "invalid locator strategy: {other}"
        ))),
    }
}

/// JS string literal (double-quoted, escaped) for embedding a selector/value in eval.
fn js_string(s: &str) -> String {
    Json::Str(s.to_string()).to_string()
}

fn find_element(id: &str, body: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let parsed = parse(body).unwrap_or(Json::Null);
    let using = parsed
        .get("using")
        .and_then(|v| v.as_str())
        .unwrap_or("css selector");
    let value = parsed
        .get("value")
        .and_then(|v| v.as_str())
        .ok_or_else(|| WdError::invalid_argument("missing value"))?;

    let find_expr = element_find_expr(using, value, false)?;
    with_session(id, sessions, |s| {
        let raw = s.engine.console_eval(&find_expr);
        let v = parse(&raw)
            .ok_or_else(|| WdError::javascript_error(format!("bad find result: {raw}")))?;
        if v == Json::Null {
            return Err(WdError::no_such_element());
        }
        Ok(v)
    })
}

fn find_elements(id: &str, body: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let parsed = parse(body).unwrap_or(Json::Null);
    let using = parsed
        .get("using")
        .and_then(|v| v.as_str())
        .unwrap_or("css selector");
    let value = parsed
        .get("value")
        .and_then(|v| v.as_str())
        .ok_or_else(|| WdError::invalid_argument("missing value"))?;

    let find_expr = element_find_expr(using, value, true)?;
    with_session(id, sessions, |s| {
        let raw = s.engine.console_eval(&find_expr);
        parse(&raw).ok_or_else(|| WdError::javascript_error(format!("bad find result: {raw}")))
    })
}

/// Build the eval expression that finds element(s) and serializes them to element ref(s).
/// `all` selects querySelectorAll vs querySelector. Link-text strategies scan by text.
fn element_find_expr(using: &str, value: &str, all: bool) -> Result<String, WdError> {
    let serialize = SERIALIZE_PREAMBLE;
    if using == "link text" || using == "partial link text" {
        let partial = using == "partial link text";
        let val = js_string(value);
        let matcher = if partial {
            format!("(a.textContent||'').indexOf({val}) !== -1")
        } else {
            format!("(a.textContent||'').trim() === {val}.trim()")
        };
        if all {
            return Ok(format!(
                "{serialize}\n(function(){{ var as = document.querySelectorAll('a'); var out = []; for (var i=0;i<as.length;i++){{ var a=as[i]; if ({matcher}) out.push(a); }} return window.__wd_serialize(out); }})()"
            ));
        }
        return Ok(format!(
            "{serialize}\n(function(){{ var as = document.querySelectorAll('a'); for (var i=0;i<as.length;i++){{ var a=as[i]; if ({matcher}) return window.__wd_serialize(a); }} return JSON.stringify(null); }})()"
        ));
    }

    let sel = selector_for(using, value)?;
    let sel_js = js_string(&sel);
    if all {
        Ok(format!(
            "{serialize}\n(function(){{ var ns = document.querySelectorAll({sel_js}); var out = []; for (var i=0;i<ns.length;i++) out.push(ns[i]); return window.__wd_serialize(out); }})()"
        ))
    } else {
        Ok(format!(
            "{serialize}\n(function(){{ var n = document.querySelector({sel_js}); return window.__wd_serialize(n); }})()"
        ))
    }
}

/// JS expression that resolves an element handle to the live node, or `null` if stale/unknown.
fn handle_node_js(eid: &str) -> Result<String, WdError> {
    if !eid.chars().all(|c| c.is_ascii_digit()) {
        return Err(WdError::no_such_element());
    }
    Ok(format!("((window.__wd_elements||[])[{eid}])"))
}

/// Run an eval that reads a string property from the resolved element. Returns `no such element`
/// if the handle does not resolve to a live node.
fn element_eval(id: &str, eid: &str, expr_on_node: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let node = handle_node_js(eid)?;
    // `__n` is the node; if null → sentinel; else evaluate expr and JSON-stringify it.
    let wrapped = format!(
        "(function(){{ var __n = {node}; if (!__n) return '__WD_NOELEM__'; try {{ var __v = (function(__el){{ return ({expr_on_node}); }})(__n); return JSON.stringify(__v === undefined ? null : __v); }} catch(e){{ return '__WD_ERR__' + (e && e.message ? e.message : String(e)); }} }})()"
    );
    with_session(id, sessions, |s| {
        let raw = s.engine.console_eval(&wrapped);
        if raw == "__WD_NOELEM__" {
            return Err(WdError::no_such_element());
        }
        if let Some(msg) = raw.strip_prefix("__WD_ERR__") {
            return Err(WdError::javascript_error(msg.to_string()));
        }
        parse(&raw).ok_or_else(|| WdError::javascript_error(format!("bad element result: {raw}")))
    })
}

fn element_text(id: &str, eid: &str, sessions: &Mutex<Sessions>) -> WdResult {
    // Prefer innerText; fall back to textContent.
    element_eval(
        id,
        eid,
        "(__el.innerText !== undefined && __el.innerText !== null ? __el.innerText : (__el.textContent||''))",
        sessions,
    )
}

fn element_attribute(id: &str, eid: &str, name: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let n = js_string(name);
    element_eval(id, eid, &format!("__el.getAttribute({n})"), sessions)
}

fn element_property(id: &str, eid: &str, name: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let n = js_string(name);
    element_eval(id, eid, &format!("__el[{n}]"), sessions)
}

fn element_css(id: &str, eid: &str, prop: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let p = js_string(prop);
    element_eval(
        id,
        eid,
        &format!(
            "(window.getComputedStyle ? window.getComputedStyle(__el).getPropertyValue({p}) : '')"
        ),
        sessions,
    )
}

fn element_name(id: &str, eid: &str, sessions: &Mutex<Sessions>) -> WdResult {
    element_eval(id, eid, "(__el.tagName||'').toLowerCase()", sessions)
}

fn element_rect(id: &str, eid: &str, sessions: &Mutex<Sessions>) -> WdResult {
    element_eval(
        id,
        eid,
        "(function(){ var r = __el.getBoundingClientRect(); return {x: r.left, y: r.top, width: r.width, height: r.height}; })()",
        sessions,
    )
}

/// Perform an Actions sequence (`POST /session/:id/actions`). We model the common case WPT's
/// testdriver uses: one or more `pointer` input sources whose actions are `pointerMove` /
/// `pointerDown` / `pointerUp` (plus `pause`, which we treat as instantaneous). Each tick across all
/// sources is replayed in source order as synthetic mouse events against the live page — move →
/// `mousemove`/hover, down → `mousedown`, up → `mouseup` + a full `click` (with the engine's focus /
/// checkbox / submit side effects). Key/wheel sources and non-default coordinate origins beyond
/// `viewport`/`pointer` are ignored (best-effort); the command still succeeds so the test proceeds.
fn perform_actions(id: &str, body: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let parsed = parse(body).unwrap_or(Json::Null);
    let sources = match parsed.get("actions").and_then(|a| a.as_array()) {
        Some(s) => s,
        None => return Ok(Json::Null),
    };
    with_session(id, sessions, |s| {
        // Build layout so hit-testing in dispatch_* works (mirrors element_click).
        let _ = s.engine.render();
        // Current pointer position in CSS px (viewport-relative); shared across pointer sources.
        let mut px = 0f64;
        let mut py = 0f64;
        for source in sources {
            if source.get("type").and_then(|t| t.as_str()) != Some("pointer") {
                continue; // key / wheel / none input sources: not modeled
            }
            let items = match source.get("actions").and_then(|a| a.as_array()) {
                Some(items) => items,
                None => continue,
            };
            for item in items {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("pointerMove") => {
                        let x = item.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let y = item.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        // origin: "viewport" (default) → absolute; "pointer" → relative to current.
                        // Element origins aren't modeled; treat them as viewport.
                        match item.get("origin").and_then(|o| o.as_str()) {
                            Some("pointer") => {
                                px += x;
                                py += y;
                            }
                            _ => {
                                px = x;
                                py = y;
                            }
                        }
                        s.engine
                            .dispatch_move(px as f32 * s.scale, py as f32 * s.scale);
                    }
                    Some("pointerDown") => {
                        s.engine.dispatch_mouse(
                            "mousedown",
                            px as f32 * s.scale,
                            py as f32 * s.scale,
                        );
                    }
                    Some("pointerUp") => {
                        // A down→up on a target is a click: fire mouseup, then a full click.
                        s.engine.dispatch_mouse(
                            "mouseup",
                            px as f32 * s.scale,
                            py as f32 * s.scale,
                        );
                        s.engine
                            .dispatch_click(px as f32 * s.scale, py as f32 * s.scale);
                    }
                    _ => {} // pause / pointerCancel / unknown: nothing to dispatch
                }
            }
        }
        // Let event handlers' microtasks/timers settle so the test observes the result.
        for _ in 0..5 {
            s.engine.tick();
        }
        Ok(Json::Null)
    })
}

fn element_click(id: &str, eid: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let node = handle_node_js(eid)?;
    // Read the rect center in CSS px.
    let rect_expr = format!(
        "(function(){{ var __n = {node}; if (!__n) return '__WD_NOELEM__'; var r = __n.getBoundingClientRect(); return JSON.stringify({{x: r.left + r.width/2, y: r.top + r.height/2}}); }})()"
    );
    with_session(id, sessions, |s| {
        // Ensure layout is built so dispatch_click can hit-test.
        let _ = s.engine.render();
        let raw = s.engine.console_eval(&rect_expr);
        if raw == "__WD_NOELEM__" {
            return Err(WdError::no_such_element());
        }
        let v = parse(&raw).ok_or_else(|| WdError::javascript_error("bad rect"))?;
        let cx = v.get("x").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
        let cy = v.get("y").and_then(|y| y.as_f64()).unwrap_or(0.0) as f32;
        // dispatch_click takes device px; rect is CSS px, so multiply by scale.
        s.engine.dispatch_click(cx * s.scale, cy * s.scale);
        for _ in 0..5 {
            s.engine.tick();
        }
        Ok(Json::Null)
    })
}

/// Send keys to an element. Best-effort: focus, set `.value` (appending), and fire `input`/`change`.
/// Note: this does not go through a real per-keystroke key event path — documented as best-effort.
fn element_value(id: &str, eid: &str, body: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let parsed = parse(body).unwrap_or(Json::Null);
    let text = parsed
        .get("text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| WdError::invalid_argument("missing text"))?;
    let node = handle_node_js(eid)?;
    let text_js = js_string(text);
    let expr = format!(
        r#"(function(){{ var __n = {node}; if (!__n) return '__WD_NOELEM__';
  try {{
    if (typeof __n.focus === 'function') __n.focus();
    if ('value' in __n) {{ __n.value = (__n.value || '') + {text_js}; }}
    else if (__n.isContentEditable) {{ __n.textContent = (__n.textContent||'') + {text_js}; }}
    var ev = (typeof Event === 'function') ? new Event('input', {{bubbles:true}}) : null;
    if (ev && __n.dispatchEvent) __n.dispatchEvent(ev);
    var ev2 = (typeof Event === 'function') ? new Event('change', {{bubbles:true}}) : null;
    if (ev2 && __n.dispatchEvent) __n.dispatchEvent(ev2);
    return 'null';
  }} catch(e){{ return '__WD_ERR__' + (e && e.message ? e.message : String(e)); }}
}})()"#
    );
    with_session(id, sessions, |s| {
        let raw = s.engine.console_eval(&expr);
        if raw == "__WD_NOELEM__" {
            return Err(WdError::no_such_element());
        }
        if let Some(msg) = raw.strip_prefix("__WD_ERR__") {
            return Err(WdError::javascript_error(msg.to_string()));
        }
        for _ in 0..3 {
            s.engine.tick();
        }
        Ok(Json::Null)
    })
}

// ---------------------------------------------------------------------------------------------
// Screenshots
// ---------------------------------------------------------------------------------------------

/// Encode the current framebuffer to a base64 PNG string.
fn framebuffer_png_base64(engine: &mut engine::Engine) -> Result<String, WdError> {
    let fb = engine.render();
    let (w, h, stride) = (fb.width, fb.height, fb.stride);
    // Repack to tight RGBA (stride may exceed width*4).
    let mut img = image::RgbaImage::new(w.max(1), h.max(1));
    for y in 0..h {
        for x in 0..w {
            let i = (y * stride + x * 4) as usize;
            if i + 3 < fb.pixels.len() {
                img.put_pixel(
                    x,
                    y,
                    image::Rgba([
                        fb.pixels[i],
                        fb.pixels[i + 1],
                        fb.pixels[i + 2],
                        fb.pixels[i + 3],
                    ]),
                );
            }
        }
    }
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| WdError::new(500, "unknown error", format!("png encode: {e}")))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(&png))
}

fn screenshot(id: &str, sessions: &Mutex<Sessions>) -> WdResult {
    with_session(id, sessions, |s| {
        let b64 = framebuffer_png_base64(&mut s.engine)?;
        Ok(Json::Str(b64))
    })
}

/// Best-effort element screenshot: full-page render cropped to the element's rect.
fn element_screenshot(id: &str, eid: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let node = handle_node_js(eid)?;
    let rect_expr = format!(
        "(function(){{ var __n = {node}; if (!__n) return '__WD_NOELEM__'; var r = __n.getBoundingClientRect(); return JSON.stringify({{x: r.left, y: r.top, width: r.width, height: r.height}}); }})()"
    );
    with_session(id, sessions, |s| {
        let _ = s.engine.render();
        let raw = s.engine.console_eval(&rect_expr);
        if raw == "__WD_NOELEM__" {
            return Err(WdError::no_such_element());
        }
        let r = parse(&raw).ok_or_else(|| WdError::javascript_error("bad rect"))?;
        let rx =
            (r.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0) * s.scale as f64).max(0.0) as u32;
        let ry =
            (r.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0) * s.scale as f64).max(0.0) as u32;
        let rw = (r.get("width").and_then(|v| v.as_f64()).unwrap_or(0.0) * s.scale as f64).max(1.0)
            as u32;
        let rh = (r.get("height").and_then(|v| v.as_f64()).unwrap_or(0.0) * s.scale as f64).max(1.0)
            as u32;

        let fb = s.engine.render();
        let (fw, fh, stride) = (fb.width, fb.height, fb.stride);
        let cw = rw.min(fw.saturating_sub(rx)).max(1);
        let ch = rh.min(fh.saturating_sub(ry)).max(1);
        let mut img = image::RgbaImage::new(cw, ch);
        for y in 0..ch {
            for x in 0..cw {
                let sx = rx + x;
                let sy = ry + y;
                let i = (sy * stride + sx * 4) as usize;
                if sx < fw && sy < fh && i + 3 < fb.pixels.len() {
                    img.put_pixel(
                        x,
                        y,
                        image::Rgba([
                            fb.pixels[i],
                            fb.pixels[i + 1],
                            fb.pixels[i + 2],
                            fb.pixels[i + 3],
                        ]),
                    );
                }
            }
        }
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(|e| WdError::new(500, "unknown error", format!("png encode: {e}")))?;
        Ok(Json::Str(
            base64::engine::general_purpose::STANDARD.encode(&png),
        ))
    })
}

// ---------------------------------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------------------------------

fn window_rect(id: &str, sessions: &Mutex<Sessions>) -> WdResult {
    with_session(id, sessions, |s| {
        Ok(obj(vec![
            ("x", Json::Num(0.0)),
            ("y", Json::Num(0.0)),
            ("width", Json::Num(s.width as f64)),
            ("height", Json::Num(s.height as f64)),
        ]))
    })
}

fn set_window_rect(id: &str, body: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let parsed = parse(body).unwrap_or(Json::Null);
    with_session(id, sessions, |s| {
        if let Some(w) = parsed.get("width").and_then(|v| v.as_f64()) {
            s.width = (w as u32).max(1);
        }
        if let Some(h) = parsed.get("height").and_then(|v| v.as_f64()) {
            s.height = (h as u32).max(1);
        }
        s.engine.set_viewport(s.width, s.height, s.scale);
        Ok(obj(vec![
            ("x", Json::Num(0.0)),
            ("y", Json::Num(0.0)),
            ("width", Json::Num(s.width as f64)),
            ("height", Json::Num(s.height as f64)),
        ]))
    })
}

// ---------------------------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------------------------

fn get_window_handle(id: &str, sessions: &Mutex<Sessions>) -> WdResult {
    with_session(id, sessions, |s| Ok(Json::Str(s.handle.clone())))
}

fn window_handles(id: &str, sessions: &Mutex<Sessions>) -> WdResult {
    with_session(id, sessions, |s| {
        Ok(Json::Arr(s.order.iter().cloned().map(Json::Str).collect()))
    })
}

fn switch_window(id: &str, body: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let parsed = parse(body).unwrap_or(Json::Null);
    let handle = parsed
        .get("handle")
        .and_then(|v| v.as_str())
        .ok_or_else(|| WdError::invalid_argument("missing handle"))?
        .to_string();
    with_session(id, sessions, |s| {
        s.switch_to(&handle)?;
        Ok(Json::Null)
    })
}

fn new_window_cmd(id: &str, body: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let parsed = parse(body).unwrap_or(Json::Null);
    // We have no real tabs-vs-windows distinction; echo back whatever was asked for (default "tab").
    let type_hint = parsed
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("tab")
        .to_string();
    with_session(id, sessions, |s| {
        let handle = s.new_window();
        Ok(obj(vec![
            ("handle", Json::Str(handle)),
            ("type", Json::Str(type_hint)),
        ]))
    })
}

fn close_window(id: &str, sessions: &Mutex<Sessions>) -> WdResult {
    with_session(id, sessions, |s| {
        let remaining = s.close_current();
        Ok(Json::Arr(remaining.into_iter().map(Json::Str).collect()))
    })
}

// ---------------------------------------------------------------------------------------------
// Timeouts
// ---------------------------------------------------------------------------------------------

fn set_timeouts(id: &str, body: &str, sessions: &Mutex<Sessions>) -> WdResult {
    let parsed = parse(body).unwrap_or(Json::Null);
    with_session(id, sessions, |s| {
        // `null` script timeout means "no timeout"; represent that as a large value.
        if let Some(script) = parsed.get("script") {
            s.script_timeout_ms = script.as_f64().map(|v| v as u64).unwrap_or(u64::MAX);
        }
        Ok(Json::Null)
    })
}

fn get_timeouts(id: &str, sessions: &Mutex<Sessions>) -> WdResult {
    with_session(id, sessions, |s| {
        Ok(obj(vec![
            ("script", Json::Num(s.script_timeout_ms as f64)),
            ("pageLoad", Json::Num(300_000.0)),
            ("implicit", Json::Num(0.0)),
        ]))
    })
}
