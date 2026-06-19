//! C ABI for the browser engine. This is the only crate the Swift shell links against.
//!
//! Ownership: `browser_engine_new` returns an owning handle that must be released with
//! `browser_engine_free`. The pixel pointer in the `Framebuffer` returned by
//! `browser_engine_render` is owned by the engine and stays valid until the next
//! `browser_engine_render` or `browser_engine_free` call on the same handle.

use std::ffi::c_char;
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
        Framebuffer { pixels: std::ptr::null(), width: 0, height: 0, stride: 0 }
    }
}

/// Create a new engine. Release with [`browser_engine_free`].
#[no_mangle]
pub extern "C" fn browser_engine_new() -> *mut Engine {
    Box::into_raw(Box::new(Engine {
        inner: engine::Engine::new(),
        last_link: None,
        last_title: None,
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

/// Navigate to `url` (NUL-terminated UTF-8). Returns 0 on success, negative on error:
/// -1 fetch/network failure, -2 bad arguments.
///
/// # Safety
/// `engine` must be a valid handle; `url` must be a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn browser_engine_load_url(engine: *mut Engine, url: *const c_char) -> i32 {
    let Some(e) = engine.as_mut() else { return -2 };
    if url.is_null() {
        return -2;
    }
    match CStr::from_ptr(url).to_str() {
        Ok(s) => e.inner.load_url(s),
        Err(_) => -2,
    }
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

/// The loaded page's `<title>` as a NUL-terminated UTF-8 C string, or null if none.
///
/// Lifetime: owned by the engine handle (stored in `last_title`); valid until the next
/// `browser_engine_title` call on this handle or until `browser_engine_free`. Copy before reusing.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_title(engine: *mut Engine) -> *const c_char {
    let Some(e) = engine.as_mut() else { return std::ptr::null() };
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

/// Paint the current state and return a borrowed view of the framebuffer.
/// Valid until the next render/free on this handle.
///
/// # Safety
/// `engine` must be a valid handle from [`browser_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn browser_engine_render(engine: *mut Engine) -> Framebuffer {
    let Some(e) = engine.as_mut() else { return Framebuffer::empty() };
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
    let Some(e) = engine.as_mut() else { return std::ptr::null() };
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
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.inner.dispatch_click(x, y))) {
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
    if e.inner.has_text_focus() { 1 } else { 0 }
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
    let key = if key.is_null() { return 0 } else { std::ffi::CStr::from_ptr(key).to_string_lossy().into_owned() };
    let code = if code.is_null() { String::new() } else { std::ffi::CStr::from_ptr(code).to_string_lossy().into_owned() };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.inner.dispatch_key(&key, &code))) {
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
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.inner.dispatch_mouse(&kind, x, y))) {
        Ok(true) => 1,
        _ => 0,
    }
}
