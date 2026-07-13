# webdriver

A **W3C WebDriver** server for our from-scratch browser engine. It is a thin protocol adapter:
it speaks the WebDriver HTTP/JSON wire protocol and drives one [`engine::Engine`] per session. It
runs **headless** (no window) and is intended as the basis for driving WPT's `wptrunner` and
reftests with standard WebDriver clients.

## Run

```sh
cargo run -p webdriver -- --port 4444     # default port is 4444
```

The server logs `webdriver listening on http://127.0.0.1:<port>` and then serves WebDriver
commands. Point any WebDriver client (Selenium, `wptrunner`, raw curl) at that base URL.

### curl smoke test

```sh
# New session
curl -s -X POST localhost:4444/session \
  -H 'Content-Type: application/json' -d '{"capabilities":{"alwaysMatch":{}}}'
# -> {"value":{"capabilities":{...},"sessionId":"browser-wd-00000001"}}

# Execute script (returns 1+1)
curl -s -X POST localhost:4444/session/browser-wd-00000001/execute/sync \
  -H 'Content-Type: application/json' -d '{"script":"return 1+1;","args":[]}'
# -> {"value":2}
```

## Design

- **HTTP server**: a tiny blocking HTTP/1.1 server built on `std::net` (one request per
  connection, `Connection: close`), modeled on `crates/wpt-runner`'s static server. Each session's
  `Engine` owns a V8 isolate that is **not `Send`**, so the server is **single-threaded**: it
  handles one connection at a time and serializes all engine work behind one `Mutex`. WebDriver
  clients issue commands sequentially, so this is sufficient (and `wptrunner` drives one session at
  a time).
- **JSON**: a small dependency-free parser/serializer (`src/json.rs`) — no `serde`.
- **Element handles**: each session keeps a JS-side registry `window.__wd_elements` (an array).
  Finding an element pushes the live node and returns its index as the handle string; the W3C
  element reference is `{"element-6066-11e4-a52e-4f735466cecf": "<index>"}`. Commands that take a
  handle resolve it with `(window.__wd_elements||[])[<handle>]`. Stale-element detection is
  minimal: a handle that no longer resolves to a live node yields `no such element`.
- **execute/sync**: the script runs as a function body —
  `(function(){ <script> }).apply(null, <args>)` — with element-reference args rewritten back to
  the live node before the call. The return value is serialized to JSON **inside JS** (DOM nodes
  become element references) and parsed back out.
- **execute/async**: the script's last argument is a completion callback we inject; it stashes the
  serialized result on `window.__wd_async_result` and sets `window.__wd_async_done`. The server
  then **ticks the engine's event loop** until the callback fires (or a ~20s `script timeout`).
  This is exactly how `wptrunner` collects `testharness.js` results.
- **Screenshots**: `engine.render()` produces an RGBA `Framebuffer`; we repack it to a tight
  `image::RgbaImage` (the framebuffer stride may exceed `width*4`), encode PNG via the `image`
  crate (already an engine dependency), and base64-encode it.
- **Initial document**: a new session (and each new window) starts on `about:blank` — a real empty
  HTML document the `net` layer serves for `about:blank`/`about:` — so script execution / find work
  before the first navigation.
- **Windows**: a session holds the current window inline plus any number of parked background
  windows, each its own headless `Engine`. `New Window` spins up a fresh `about:blank` engine;
  `Close Window` drops it and promotes a survivor. This backs wptrunner's per-test window.

## Supported commands

| Command | Method + path | Status |
|---|---|---|
| Status | `GET /status` | ✅ |
| New Session | `POST /session` | ✅ (accepts any caps; reads window size if offered) |
| Delete Session | `DELETE /session/{id}` | ✅ |
| Navigate To | `POST /session/{id}/url` | ✅ (ticks until `readyState==="complete"` or timeout) |
| Get Current URL | `GET /session/{id}/url` | ✅ |
| Get Title | `GET /session/{id}/title` | ✅ |
| Get Page Source | `GET /session/{id}/source` | ✅ (`document.documentElement.outerHTML`) |
| Refresh | `POST /session/{id}/refresh` | ✅ (reloads current URL) |
| Back | `POST /session/{id}/back` | ✅ |
| Forward | `POST /session/{id}/forward` | ✅ |
| Execute Script | `POST /session/{id}/execute/sync` | ✅ |
| Execute Async Script | `POST /session/{id}/execute/async` | ✅ |
| Find Element | `POST /session/{id}/element` | ✅ (`css selector`, `tag name`, `id`, `class name`, `link text`, `partial link text`) |
| Find Elements | `POST /session/{id}/elements` | ✅ |
| Get Element Text | `GET /session/{id}/element/{eid}/text` | ✅ (`innerText`/`textContent`) |
| Get Element Attribute | `GET /session/{id}/element/{eid}/attribute/{name}` | ✅ |
| Get Element Property | `GET /session/{id}/element/{eid}/property/{name}` | ✅ |
| Get Element CSS Value | `GET /session/{id}/element/{eid}/css/{prop}` | ✅ (`getComputedStyle`) |
| Get Element Tag Name | `GET /session/{id}/element/{eid}/name` | ✅ |
| Get Element Rect | `GET /session/{id}/element/{eid}/rect` | ✅ (`getBoundingClientRect`) |
| Element Click | `POST /session/{id}/element/{eid}/click` | ✅ (rect center → `dispatch_click`) |
| Element Send Keys | `POST /session/{id}/element/{eid}/value` | ⚠️ **best-effort** (focus + set `.value` + fire `input`/`change`; not a real per-keystroke key path) |
| Take Screenshot | `GET /session/{id}/screenshot` | ✅ (base64 PNG) |
| Take Element Screenshot | `GET /session/{id}/element/{eid}/screenshot` | ⚠️ **best-effort** (full-page render cropped to the element rect) |
| Get Window Rect | `GET /session/{id}/window/rect` | ✅ |
| Set Window Rect | `POST /session/{id}/window/rect` | ✅ (`set_viewport`) |

### Errors

Responses are `{"value": <result>}` with HTTP 200 on success. Errors are
`{"value":{"error":"<code>","message":"<msg>","stacktrace":""}}` with the status:
`404` for `invalid session id` / `no such element` / `unknown command`, `400` for
`invalid argument`, `500` for `javascript error` / `script timeout` / `unknown error`.

## Known limitations / stubs (easy to find later)

- **Send keys** (`element/value`) is best-effort: it sets `.value` and fires `input`/`change`
  rather than dispatching real key events per character.
- **Element screenshot** crops the full-page framebuffer to the element rect rather than rendering
  the element in isolation.
- **`data:` URLs** are not supported by the `net` layer; navigate to `http(s)://`, `file://`, or
  `about:blank`.
