//! C ABI for the browser engine. This is the only crate the Swift shell links against.
//!
//! Ownership: `browser_engine_new` returns an owning handle that must be released with
//! `browser_engine_free`. The pixel pointer in the `Framebuffer` returned by
//! `browser_engine_render` is owned by the engine and stays valid until the next
//! `browser_engine_render` or `browser_engine_free` call on the same handle.

use std::ffi::c_char;
use std::ffi::c_void;
use std::ffi::CStr;
use std::ffi::CString;

/// Opaque engine handle. cbindgen emits this as a forward-declared struct.
///
/// `last_link` retains the most recent `browser_engine_link_at` result so the `*const c_char`
/// returned to the caller stays valid until the next `browser_engine_link_at` call on this handle
/// (or until `browser_engine_free`).
pub struct Engine {
    inner: engine::Engine,
    last_link: Option<CString>,
    last_title: Option<CString>,
    last_url: Option<CString>,
    last_eval: Option<CString>,
    last_console: Option<CString>,
    last_netlog: Option<CString>,
    last_select: Option<CString>,
    last_selected: Option<CString>,
    last_domtree: Option<CString>,
}

/// A borrowed view of the engine's RGBA8 (straight-alpha) framebuffer.
/// `stride` is bytes per row. A null `pixels` means "nothing rendered".
#[repr(C)]
pub struct Framebuffer {
    pub pixels: *const u8,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl Framebuffer {
    fn empty() -> Self {
        Framebuffer {
            pixels: std::ptr::null(),
            width: 0,
            height: 0,
            stride: 0,
        }
    }
}

/// Create a new engine. Release with [`browser_engine_free`].
#[no_mangle]
pub extern "C" fn browser_engine_new() -> *mut Engine {
    Box::into_raw(Box::new(Engine {
        inner: engine::Engine::new(),
        last_link: None,
        last_title: None,
        last_url: None,
        last_eval: None,
        last_console: None,
        last_netlog: None,
        last_select: None,
        last_selected: None,
        last_domtree: None,
    }))
}

/// Free an engine created by [`browser_engine_new`]. Null is a no-op.
///
/// # Safety
/// `engine` must be a pointer returned by [`browser_engine_new`] and not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn browser_engine_free(engine: *mut Engine) {
    if !engine.is_null() {
        drop(Box::from_raw(engine));
    }
}

/// Set the logical viewport size (points) and backing scale (e.g. 2.0 on Retina).
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_set_viewport(
    engine: *mut Engine,
    width: u32,
    height: u32,
    scale: f32,
) {
    if let Some(e) = engine.as_mut() {
        e.inner.set_viewport(width, height, scale);
    }
}

/// Set the effective OS appearance (`is_dark != 0` = Dark). Surfaces `prefers-color-scheme` to page
/// JS (`matchMedia('(prefers-color-scheme: dark)')`, firing `change` on existing MediaQueryLists)
/// and the CSS `@media (prefers-color-scheme)` cascade. The host should call this on launch and
/// whenever the user toggles Light/Dark, then re-render.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_set_color_scheme(engine: *mut Engine, is_dark: bool) {
    if let Some(e) = engine.as_mut() {
        e.inner.set_color_scheme(is_dark);
    }
}

/// Install (or clear, with a null `cb`) the progressive-load frame callback. While set, the engine
/// invokes `cb(ctx, framebuffer)` SYNCHRONOUSLY from inside `browser_engine_load_url`, on the load
/// thread, each time it paints a partial frame as the page's HTML streams in (and once more for the
/// final frame). The `Framebuffer` pixels point at the engine's own buffer and are valid ONLY for
/// the duration of the callback call — copy them synchronously; do not retain the pointer. `ctx` is
/// passed through unchanged. Pass a null `cb` to disable progressive frames (the default).
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`]; `cb` (if non-null) must remain a
/// valid function pointer and `ctx` valid for the lifetime of every `browser_engine_load_url` call
/// made while the callback is installed.
#[no_mangle]
pub unsafe extern "C" fn browser_engine_set_progress_callback(
    engine: *mut Engine,
    cb: Option<extern "C" fn(*mut c_void, engine::FrameView)>,
    ctx: *mut c_void,
) {
    let Some(e) = engine.as_mut() else { return };
    e.inner.set_frame_callback(cb.map(|f| (f, ctx)));
}

/// Navigate to `url` (NUL-terminated UTF-8). Returns 0 on success, negative on error:
/// -1 fetch/network failure, -2 bad arguments.
///
/// When a progress callback is installed via [`browser_engine_set_progress_callback`], this paints
/// and delivers partial frames synchronously as the page streams in, then the final frame.
///
/// # Safety
/// `engine` must be a valid handle; `url` must be a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn browser_engine_load_url(engine: *mut Engine, url: *const c_char) -> i32 {
    let Some(e) = engine.as_mut() else { return -2 };
    if url.is_null() {
        return -2;
    }
    let s = match CStr::from_ptr(url).to_str() {
        Ok(s) => s,
        Err(_) => return -2,
    };
    // Streaming runs page scripts + a user-supplied frame callback; never let a panic cross the C
    // boundary (it would abort the app). Treat a panic as a load failure.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.inner.load_url(s))).unwrap_or(-1)
}

/// Scroll the page by `dy` device pixels (positive scrolls content up / toward the end).
/// Clamped to the document bounds on the next render.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_scroll_by(engine: *mut Engine, dy: f32) {
    if let Some(e) = engine.as_mut() {
        e.inner.scroll_by(dy);
    }
}

/// V8 heap used by this tab's JS, in bytes (0 if no live session). For the tab tooltip.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_heap_bytes(engine: *mut Engine) -> u64 {
    engine.as_ref().map(|e| e.inner.heap_bytes()).unwrap_or(0)
}

/// Cumulative active JS time on this tab's thread, in nanoseconds (0 if no session). The UI samples
/// deltas over wall-clock to display a CPU %.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_cpu_ns(engine: *mut Engine) -> u64 {
    engine.as_ref().map(|e| e.inner.cpu_ns()).unwrap_or(0)
}

/// The loaded page's `<title>` as a NUL-terminated UTF-8 C string, or null if none.
///
/// Lifetime: owned by the engine handle (stored in `last_title`); valid until the next
/// `browser_engine_title` call on this handle or until `browser_engine_free`. Copy before reusing.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_title(engine: *mut Engine) -> *const c_char {
    let Some(e) = engine.as_mut() else {
        return std::ptr::null();
    };
    match e.inner.title().and_then(|s| CString::new(s).ok()) {
        Some(cstr) => {
            let ptr = cstr.as_ptr();
            e.last_title = Some(cstr);
            ptr
        }
        None => {
            e.last_title = None;
            std::ptr::null()
        }
    }
}

/// The URL currently committed in the engine — the resolved final URL after fixup, HSTS upgrade,
/// redirects, and any http fallback — as a NUL-terminated UTF-8 C string, or null if nothing has
/// loaded. Shells call this after `browser_engine_load_url` to show the real address (and to record
/// history), since the loaded URL can differ from the one passed in.
///
/// Lifetime: owned by the engine handle (stored in `last_url`); valid until the next
/// `browser_engine_current_url` call on this handle or until `browser_engine_free`. Copy before reusing.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_current_url(engine: *mut Engine) -> *const c_char {
    let Some(e) = engine.as_mut() else {
        return std::ptr::null();
    };
    match e.inner.current_url().and_then(|s| CString::new(s).ok()) {
        Some(cstr) => {
            let ptr = cstr.as_ptr();
            e.last_url = Some(cstr);
            ptr
        }
        None => {
            e.last_url = None;
            std::ptr::null()
        }
    }
}

/// The current page's favicon as a borrowed RGBA8 (straight-alpha) [`Framebuffer`] (`stride` =
/// `width * 4`), or an empty framebuffer (null `pixels`) if the page has no loadable icon. The
/// pixels are owned by the engine and stay valid until the next `browser_engine_load_url` (which
/// replaces the icon) or `browser_engine_free` — copy them synchronously.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_favicon(engine: *mut Engine) -> Framebuffer {
    let Some(e) = engine.as_mut() else {
        return Framebuffer::empty();
    };
    match e.inner.favicon() {
        Some((px, w, h)) => Framebuffer {
            pixels: px.as_ptr(),
            width: w,
            height: h,
            stride: w.saturating_mul(4),
        },
        None => Framebuffer::empty(),
    }
}

/// Paint the current state and return a borrowed view of the framebuffer.
/// Valid until the next render/free on this handle.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_render(engine: *mut Engine) -> Framebuffer {
    let Some(e) = engine.as_mut() else {
        return Framebuffer::empty();
    };
    // Panic backstop: a panic must NEVER cross this C boundary (it would abort the whole app).
    // Heavy/hostile pages can drive the engine into edge cases; on a render panic we return an
    // empty framebuffer and keep the app alive rather than crashing.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let fb = e.inner.render();
        Framebuffer {
            pixels: fb.pixels.as_ptr(),
            width: fb.width,
            height: fb.height,
            stride: fb.stride,
        }
    })) {
        Ok(fb) => fb,
        Err(_) => Framebuffer::empty(),
    }
}

/// Hit-test the most recently rendered page at framebuffer device-pixel `(x, y)`. If a link
/// (`<a href>`) is under that point, returns a NUL-terminated UTF-8 C string with the resolved
/// absolute URL; otherwise returns null.
///
/// Lifetime: the returned pointer is owned by the engine handle (stored in `last_link`) and stays
/// valid until the next `browser_engine_link_at` call on this handle, or until
/// `browser_engine_free`. Copy it (e.g. via `String(cString:)`) before calling again.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_link_at(
    engine: *mut Engine,
    x: f32,
    y: f32,
) -> *const c_char {
    let Some(e) = engine.as_mut() else {
        return std::ptr::null();
    };
    match e.inner.link_at(x, y).and_then(|s| CString::new(s).ok()) {
        Some(cstr) => {
            let ptr = cstr.as_ptr();
            // Retain so the pointer stays valid until the next call / free.
            e.last_link = Some(cstr);
            ptr
        }
        None => {
            // Drop any previously retained link; nothing here.
            e.last_link = None;
            std::ptr::null()
        }
    }
}

/// Dispatch a `click` into the live page JS at framebuffer device-pixel `(x, y)` (viewport-
/// relative). Fires the page's click handlers (with bubbling); if the DOM changed, returns 1 to
/// signal the caller should re-render. Returns 0 if nothing changed / no live runtime.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_dispatch_click(engine: *mut Engine, x: f32, y: f32) -> i32 {
    let Some(e) = engine.as_mut() else { return 0 };
    // Page JS is arbitrary; never let a panic cross the C boundary (it would abort the app).
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        e.inner.dispatch_click(x, y)
    })) {
        Ok(true) => 1,
        _ => 0,
    }
}

/// Whether a text `<input>`/`<textarea>` is currently focused (so the UI should route keystrokes
/// into the page via [`browser_engine_dispatch_key`]). Returns 1 if focused, else 0.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_has_text_focus(engine: *mut Engine) -> i32 {
    let Some(e) = engine.as_mut() else { return 0 };
    if e.inner.has_text_focus() {
        1
    } else {
        0
    }
}

/// Deliver a key press to the focused text field's page JS. `key` is the DOM key value
/// (e.g. "a", "Backspace", "Enter") and `code` the physical key code (e.g. "KeyA"), both NUL-
/// terminated UTF-8. Updates the field value + fires keydown/input/keyup. Returns 1 if the DOM
/// changed (re-render), else 0.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`]; `key`/`code` valid C strings.
#[no_mangle]
pub unsafe extern "C" fn browser_engine_dispatch_key(
    engine: *mut Engine,
    key: *const c_char,
    code: *const c_char,
) -> i32 {
    let Some(e) = engine.as_mut() else { return 0 };
    let key = if key.is_null() {
        return 0;
    } else {
        std::ffi::CStr::from_ptr(key).to_string_lossy().into_owned()
    };
    let code = if code.is_null() {
        String::new()
    } else {
        std::ffi::CStr::from_ptr(code)
            .to_string_lossy()
            .into_owned()
    };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        e.inner.dispatch_key(&key, &code)
    })) {
        Ok(true) => 1,
        _ => 0,
    }
}

/// Run any due timers / animation callbacks in the live page JS (drives `setTimeout`/`setInterval`/
/// `requestAnimationFrame` after load). Cheap no-op when nothing is due. Returns 1 if the DOM
/// changed (the caller should re-render), else 0.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_tick(engine: *mut Engine) -> i32 {
    let Some(e) = engine.as_mut() else { return 0 };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.inner.tick())) {
        Ok(true) => 1,
        _ => 0,
    }
}

/// Move the pointer to framebuffer device-pixel `(x, y)` (viewport-relative): fires the page's
/// hover events (`mouseover`/`mouseout`/`mouseenter`/`mouseleave`/`mousemove`) as the node under
/// the pointer changes. Cheap no-op when the hovered node is unchanged. Returns 1 if the DOM
/// changed (re-render), else 0.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_dispatch_move(engine: *mut Engine, x: f32, y: f32) -> i32 {
    let Some(e) = engine.as_mut() else { return 0 };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.inner.dispatch_move(x, y))) {
        Ok(true) => 1,
        _ => 0,
    }
}

/// Devtools console REPL: evaluate `code` (NUL-terminated UTF-8) in the live page JS and return a
/// NUL-terminated UTF-8 result/error string. Lifetime: owned by the engine (stored in `last_eval`),
/// valid until the next `browser_engine_console_eval` call or `browser_engine_free`.
///
/// # Safety
/// `engine` must be a valid handle; `code` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn browser_engine_console_eval(
    engine: *mut Engine,
    code: *const c_char,
) -> *const c_char {
    let Some(e) = engine.as_mut() else {
        return std::ptr::null();
    };
    if code.is_null() {
        return std::ptr::null();
    }
    let code = CStr::from_ptr(code).to_string_lossy().into_owned();
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.inner.console_eval(&code)))
            .unwrap_or_else(|_| "Uncaught Error: evaluation panicked".to_string());
    match CString::new(result) {
        Ok(cstr) => {
            let ptr = cstr.as_ptr();
            e.last_eval = Some(cstr);
            ptr
        }
        Err(_) => std::ptr::null(),
    }
}

/// The current page's console + error lines, joined by '\n', as a NUL-terminated UTF-8 string
/// (for the devtools Console tab). Owned by the engine (`last_console`); valid until the next call.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_console_text(engine: *mut Engine) -> *const c_char {
    let Some(e) = engine.as_mut() else {
        return std::ptr::null();
    };
    let text = e.inner.console_lines().join("\n");
    match CString::new(text) {
        Ok(cstr) => {
            let ptr = cstr.as_ptr();
            e.last_console = Some(cstr);
            ptr
        }
        Err(_) => std::ptr::null(),
    }
}

/// The current navigation's network activity as a JSON array string (for the devtools Network tab).
/// Owned by the engine (`last_netlog`); valid until the next call.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_network_log(engine: *mut Engine) -> *const c_char {
    let Some(e) = engine.as_mut() else {
        return std::ptr::null();
    };
    match CString::new(e.inner.network_log_json()) {
        Ok(cstr) => {
            let ptr = cstr.as_ptr();
            e.last_netlog = Some(cstr);
            ptr
        }
        Err(_) => std::ptr::null(),
    }
}

/// Dispatch a raw mouse event (`kind` = "mousedown"/"mouseup"/"dblclick"/"contextmenu", NUL-
/// terminated UTF-8) to the node at device-pixel `(x, y)`. Returns 1 if the DOM changed.
///
/// # Safety
/// `engine` must be a valid handle; `kind` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn browser_engine_dispatch_mouse(
    engine: *mut Engine,
    kind: *const c_char,
    x: f32,
    y: f32,
) -> i32 {
    let Some(e) = engine.as_mut() else { return 0 };
    if kind.is_null() {
        return 0;
    }
    let kind = CStr::from_ptr(kind).to_string_lossy().into_owned();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        e.inner.dispatch_mouse(&kind, x, y)
    })) {
        Ok(true) => 1,
        _ => 0,
    }
}

/// Minimal JSON string escaper (quotes, backslash, control chars) for embedding option labels.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Hit-test the most recently rendered page at framebuffer device-pixel `(x, y)` (viewport-
/// relative). If a `<select>` is under that point, returns a NUL-terminated UTF-8 JSON string
/// `{"id":N,"x":..,"y":..,"w":..,"h":..,"selected":K,"options":["..",..]}` (rect in device px,
/// scroll subtracted); otherwise returns null.
///
/// Lifetime: owned by the engine handle (stored in `last_select`); valid until the next
/// `browser_engine_select_at` call on this handle, or until `browser_engine_free`.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_select_at(
    engine: *mut Engine,
    x: f32,
    y: f32,
) -> *const c_char {
    let Some(e) = engine.as_mut() else {
        return std::ptr::null();
    };
    let hit = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.inner.select_at(x, y)))
        .unwrap_or_default();
    match hit {
        Some(h) => {
            let opts: Vec<String> = h
                .options
                .iter()
                .map(|o| format!("\"{}\"", json_escape(o)))
                .collect();
            let json = format!(
                "{{\"id\":{},\"x\":{},\"y\":{},\"w\":{},\"h\":{},\"selected\":{},\"options\":[{}]}}",
                h.node_id, h.x, h.y, h.width, h.height, h.selected, opts.join(",")
            );
            match CString::new(json) {
                Ok(cstr) => {
                    let ptr = cstr.as_ptr();
                    e.last_select = Some(cstr);
                    ptr
                }
                Err(_) => {
                    e.last_select = None;
                    std::ptr::null()
                }
            }
        }
        None => {
            e.last_select = None;
            std::ptr::null()
        }
    }
}

/// Pick the `index`-th `<option>` of the `<select>` whose node id is `node_id`: updates the
/// selection + `value` and fires bubbling `input`/`change` in the live page JS (so the page's
/// `change` handler runs). Returns 1 if the selection changed (re-render warranted), else 0.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_set_select_index(
    engine: *mut Engine,
    node_id: usize,
    index: usize,
) -> i32 {
    let Some(e) = engine.as_mut() else { return 0 };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        e.inner.set_select_index(node_id, index)
    })) {
        Ok(true) => 1,
        _ => 0,
    }
}

/// Begin a text selection at framebuffer device-pixel `(x, y)` (viewport-relative, scroll NOT
/// pre-added — the engine folds in the scroll offset itself, matching `browser_engine_dispatch_click`).
/// Sets the selection anchor (collapsed) at that point.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_selection_start(engine: *mut Engine, x: f32, y: f32) {
    let Some(e) = engine.as_mut() else { return };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        e.inner.selection_start(x, y)
    }));
}

/// Extend the active text selection's focus to framebuffer device-pixel `(x, y)` (viewport-relative,
/// scroll NOT pre-added). Keeps the anchor fixed; called repeatedly as the pointer drags.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_selection_extend(engine: *mut Engine, x: f32, y: f32) {
    let Some(e) = engine.as_mut() else { return };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        e.inner.selection_extend(x, y)
    }));
}

/// Clear any active text selection.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_selection_clear(engine: *mut Engine) {
    let Some(e) = engine.as_mut() else { return };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.inner.selection_clear()));
}

/// Whether there is a non-empty (non-collapsed) text selection. Returns 1 if so, else 0.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_has_selection(engine: *mut Engine) -> i32 {
    let Some(e) = engine.as_mut() else { return 0 };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.inner.has_selection())) {
        Ok(true) => 1,
        _ => 0,
    }
}

/// The currently selected page text as a NUL-terminated UTF-8 C string, or null if there is no
/// (non-collapsed) selection. Lifetime: owned by the engine handle (stored in `last_selected`);
/// valid until the next `browser_engine_selected_text` call on this handle, or `browser_engine_free`.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_selected_text(engine: *mut Engine) -> *const c_char {
    let Some(e) = engine.as_mut() else {
        return std::ptr::null();
    };
    let text = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.inner.selected_text()))
        .unwrap_or_default();
    if text.is_empty() {
        e.last_selected = None;
        return std::ptr::null();
    }
    match CString::new(text) {
        Ok(cstr) => {
            let ptr = cstr.as_ptr();
            e.last_selected = Some(cstr);
            ptr
        }
        Err(_) => {
            e.last_selected = None;
            std::ptr::null()
        }
    }
}

/// The current document's DOM tree as a NUL-terminated UTF-8 JSON string for the DevTools "Elements"
/// tab (nested `{"id","type","tag","attrs","text","children"}` objects); `"{}"` when no document is
/// loaded. Lifetime: owned by the engine handle (stored in `last_domtree`); valid until the next
/// `browser_engine_dom_tree` call on this handle, or until `browser_engine_free`.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_dom_tree(engine: *mut Engine) -> *const c_char {
    let Some(e) = engine.as_mut() else {
        return std::ptr::null();
    };
    let json = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.inner.dom_tree_json()))
        .unwrap_or_else(|_| "{}".to_string());
    match CString::new(json) {
        Ok(cstr) => {
            let ptr = cstr.as_ptr();
            e.last_domtree = Some(cstr);
            ptr
        }
        Err(_) => {
            e.last_domtree = None;
            std::ptr::null()
        }
    }
}

/// Hit-test the most recently rendered page at framebuffer device-pixel `(x, y)` (viewport-relative)
/// and return the NodeId of the nearest enclosing element, or -1 if there's no element there (no
/// layout/DOM, or the point misses all laid-out content). For the right-click "Inspect Element" flow.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_node_at_point(engine: *mut Engine, x: f32, y: f32) -> i64 {
    let Some(e) = engine.as_mut() else { return -1 };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.inner.node_at_point(x, y))) {
        Ok(Some(id)) => id as i64,
        _ => -1,
    }
}

/// Set (or clear) the DevTools "Elements" inspector highlight: when `node_id` is non-negative, the
/// next `browser_engine_render` draws a translucent overlay over that node's laid-out border box. A
/// negative `node_id` clears the highlight. An out-of-range id is ignored.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_set_inspect_node(engine: *mut Engine, node_id: i64) {
    let Some(e) = engine.as_mut() else { return };
    let node = if node_id < 0 {
        None
    } else {
        Some(node_id as usize)
    };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        e.inner.set_inspect_node(node)
    }));
}
