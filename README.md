# browser

A web browser written from scratch. macOS-only for now.

- **Swift** is the app shell: window, URL bar, navigation chrome, event loop.
- **Rust** is "the inners": the actual engine — networking, parsing, DOM, style,
  layout, and paint.

## Guiding constraint

The **eventual goal is to rewrite everything in Rust**, including the parts we currently
reuse. So every reused crate is walled off behind *our own* module boundary, and swapping
it for a hand-written implementation later is a localized change, not a refactor:

| Reused today | Hidden behind | Eventually replaced by |
|---|---|---|
| `ureq` (HTTP/TLS) | `net::fetch` → `net::Response` | hand-written HTTP/1.1 (TLS likely stays reused — DIY TLS is unsafe) |
| `fontdue` (glyph raster) | `paint::GlyphRasterizer` trait | hand-written rasterizer |
| `boa_engine` (JS VM) | `js::Runtime` / `js::eval` | hand-written JS engine |
| `cbindgen` (header gen) | build script only | n/a (build tooling) |

Everything else — HTML tokenizer/tree-builder, CSS parser, DOM, cascade, layout, the
compositor — is ours.

## Architecture

```
URL ─▶ net (fetch) ─▶ html::parse ─▶ DOM ─▶ style (cascade) ─▶ layout ─▶ paint ─▶ RGBA
                                                                                   │
                                            crates/ffi (C ABI) ◀──────────────────┘
                                                   │
                                       Swift app blits the framebuffer to a layer
```

Rust paints an RGBA framebuffer; Swift uploads it to an `NSView` via `CGImage` each frame.
This is the simplest possible Rust↔Swift boundary. (A display-list + CoreText path, or a
GPU surface, can replace it later without touching the engine.)

### Rust workspace (`crates/`)
- `net` — fetch a URL → bytes + content-type *(reuses `ureq`)*
- `html` — hand-written HTML tokenizer + tree builder → DOM
- `css` — CSS tokenizer + parser *(stub; Phase 3)*
- `dom` — arena-based node tree
- `js` — JS runtime + DOM/`window`/`self` bindings; runs page scripts *(reuses `boa_engine`)*
- `css` — hand-written CSS parser (`<style>` blocks + inline `style=""`)
- `style` — cascade (UA + author + inline) → computed styles, box + flex/grid/position props
- `layout` — block/inline/inline-block, **flexbox**, basic **grid**, and **positioning**
  (relative/absolute/fixed) → positioned box tree (`TextMeasurer` decouples fonts)
- `paint` — RGBA framebuffer + fill/text primitives; `GlyphRasterizer` trait
- `engine` — orchestrates the pipeline; produces the framebuffer
- `ffi` — C ABI (`staticlib`); `cbindgen` generates `include/browser.h`

### Swift app (`swift/`)
- `CBrowser` — system-library target wrapping `include/browser.h`
- `Browser` — AppKit app linking `libbrowser_ffi.a`

## Memory model

Tabs are **not** capped at 4 GiB. Unlike Chrome — which isolates each tab in its own
renderer process and runs V8 with pointer compression that effectively caps a tab's JS heap
near 4 GiB — every tab here is just heap inside our single **64-bit** process, and the JS
engine (Boa) uses no pointer compression. A tab is therefore limited only by the machine's
RAM + swap. We set no `rlimit`, and size types on the hot paths are 64-bit (`net`'s body
backstop sits at 16 GiB, the DOM arena indexes with `usize`).

## Build & run

```sh
bash scripts/build.sh           # builds Rust static lib + header, then the Swift app
./swift/.build/debug/Browser    # launches the window
```

Tests: `cargo test`

## Status

The app fetches a URL (`http(s)://` or `file://`) through the Rust engine, parses the HTML
into a DOM with our hand-written tokenizer/tree-builder, runs the page's inline `<script>`
tags through the embedded JS runtime (capturing `console` output), and paints the page's
visible text plus a console panel into the framebuffer the AppKit shell blits.

The shell is a **tabbed** browser: a translucent toolbar with SF Symbol back/forward/reload,
a pill address bar, per-tab engine instances + history, a tab bar with new/close, and
shortcuts (⌘T/⌘W/⌘1–9, ⌘L, ⌘R, ⌘[ / ⌘]).

Page scripts run with real **DOM bindings** (`document.getElementById`, `textContent`,
`createElement`/`appendChild`, `document.title`) and browser globals (`window`/`self`/
`globalThis`), so JS mutations show up in what's rendered. **CSS** is parsed and cascaded
(UA + `<style>` + inline `style=""`), and text is painted with the computed color, font
size, bold (faux), alignment, and `display:none`.

JS also has timers/event-loop (`setTimeout`/`setInterval`/`queueMicrotask`/`rAF`, bounded
drain). Layout is a real **box model**: block/inline boxes with width/height, margins,
padding, and borders; the engine paints backgrounds, borders, and content-box-wrapped text.

JS runs in a fuller **browser environment** (`navigator`, `location`, `localStorage`,
`history`, `matchMedia`, `getComputedStyle`, event listeners + `DOMContentLoaded`/`load`).
The engine fetches **external sub-resources** — `<link rel=stylesheet>` and `<script src>`,
resolved against the page URL and interleaved in document order — so real sites (e.g.
Wikipedia) render with their actual styling.

Layout now does **flexbox** (direction/wrap/justify/align/grow/shrink/gap), **CSS
positioning** (relative/absolute/fixed with shrink-to-fit + edge anchoring), **inline-block**,
and a **basic grid** (explicit px/fr/% tracks), with correct container-height accumulation
(siblings stack without overlap). JS DOM bindings write `style`/`classList`/attributes
through to the DOM so script-driven styling re-renders.

Real sites render: **imlunahey.com** (a Tailwind/TanStack site) renders cleanly with its
two-column layout, cards, and styling, with zero console errors. JS-app sites that build
their UI entirely at runtime (e.g. google.com — 95% `display:none` until its obfuscated JS
runs) remain out of reach without a near-complete JS/web-platform implementation.

Done: networking + external CSS/JS · HTML→DOM · tabs · JS (DOM bindings + timers + browser
env) · CSS cascade · box-model + flexbox/grid/positioning layout + paint.
Roadmap: **scrolling** · **images** · z-index paint order · floats · margin collapsing ·
fuller grid · DOM events on input · `fetch`/XHR · concurrent fetch · GPU rendering.
