# Feature backlog (non-CSS)

See also docs/CSS-TODO.md for the CSS backlog.

## Web / JS APIs
- [x] **FetchData / FormData** — DONE: net::request (GET/POST/PUT/PATCH/DELETE), fetch() sends method/headers/body, FormData API + urlencoded serialization. (Multipart + File not yet.)
  - ~~original note~~ **FetchData** — (requested by user) form/data serialization for fetch. Likely `FormData`
      (`new FormData(form)`, `.append/.get/.getAll/.entries`, iterate) so forms can be submitted
      via `fetch(url, { method, body: formData })`; also wire `<form>` submit to build it.
      Confirm exact scope with user.

## In progress (resume here)
- [ ] **google.com `devicePixelRatio` (live-only)** — Partly fixed. Root cause chain found in
  google's xjs bundle:
    - `Qwa = function(a){ this.oa = a ? a.getWindow() : window; this.Aa = this.oa.devicePixelRatio... }`
    - `getWindow = function(){ return this.ka.defaultView }`  (this.ka = a Document)
  We added `document.defaultView = window`, which fixed the `_.ai(doc).devicePixelRatio` path,
  but the LIVE bundle still throws via `Qwa` → `getWindow` → `this.ka.defaultView` (a DomHelper's
  document whose `.defaultView` is still undefined).
  **Key gap:** headless `engine` load of google shows 0 devicePixelRatio errors (after 40 ticks),
  but the live app DOES throw (stack: new Qwa → _.bi → ... → _._ModuleManager_initialize).
  → TOMORROW: figure out why headless differs from live (different google bundle? more scripts
    loaded live? viewport?), reproduce headless, then ensure every document-like object's
    `defaultView` points at window (incl. DomHelper docs / any doc google constructs). google is
    the all-or-nothing hardest target — after this it still has `_._DumpException` + module-loader
    errors, so it won't fully render regardless.
