# browser

A web browser written from scratch.

- **Rust** is "the inners": the actual engine — networking, parsing, DOM, style,
  layout, and paint. The engine is **platform-agnostic** and produces an RGBA framebuffer.
- The **app shell** (window, URL bar, tabs, event loop) is a thin **native** layer per OS:
  - **macOS** — Swift / AppKit *(current)*
  - **Linux** — GTK4 *(planned)*
  - **Windows** — Win32 *(planned)*

## Guiding constraint

The **eventual goal is to rewrite everything in Rust**, including the parts we currently
reuse. So every reused crate is walled off behind *our own* module boundary, and swapping
it for a hand-written implementation later is a localized change, not a refactor:

| Reused today | Hidden behind | Eventually replaced by |
|---|---|---|
| `ureq` (HTTP/TLS) | `net::fetch` → `net::Response` | hand-written HTTP/1.1 (TLS likely stays reused — DIY TLS is unsafe) |
| `fontdue` (glyph raster) | `paint::GlyphRasterizer` trait | hand-written rasterizer |
| `v8` (JS VM) | `js::Runtime` / `js::eval` | hand-written JS engine |
| `cbindgen` (header gen) | build script only | n/a (build tooling) |

Everything else — HTML tokenizer/tree-builder, CSS parser, DOM, cascade, layout, the
compositor — is ours.

## Build & run

### Prerequisites

- **Rust** (stable) — install via [rustup](https://rustup.rs).
- For App Shell: **Xcode command-line tools** (`xcode-select --install`)

### Engine / tests only

The engine is plain Rust and builds on **macOS, Linux, and Windows**:

```sh
cargo build --workspace # Build
cargo test --workspace  # Build and run tests
```

### The App shell (macOS only)

The app shell currently ships for **macOS** (Swift / AppKit), so a graphical browser today requires macOS.
Run from the repo root:

```sh
bash scripts/build.sh              # debug: Rust lib + the Swift app
./swift/.build/debug/Browser       # launch

bash scripts/build.sh release      # optimized, bare executable
./swift/.build/release/Browser     # launch

bash scripts/build.sh release-app  # optimized + packaged + signed dist/Browser.app
open dist/Browser.app              # launch
```

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
- `js` — JS runtime + DOM/`window`/`self` bindings; runs page scripts *(reuses `v8`)*
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
engine (V8) uses no pointer compression. A tab is therefore limited only by the machine's
RAM + swap. We set no `rlimit`, and size types on the hot paths are 64-bit (`net`'s body
backstop sits at 16 GiB, the DOM arena indexes with `usize`).

## Contributing

This project is built **LLM-first**. We strongly prefer changes authored by a capable coding
model — **Claude 4.8** or **GPT-5.5** — over hand-written code. The engine is large and intricate,
and the agents are faster and more thorough here. Drive a model, review its work, and ship that.
Note the model/tooling on your PR.

- **PRs only.** `main` is protected; every change lands through a pull request.
- **Conventional Commits.** The PR title must be `feat(...)`, `fix(...)`, `ci: …`, etc. CI enforces
  it, and `release-plz` uses it to bump versions and write CHANGELOGs.
- **CI on every PR:** `cargo test` on macOS / Linux / Windows, `fmt` + `clippy`, and the **WPT
  conformance suite** (via the real `wpt run` harness over WebDriver) — it writes a pass-rate
  summary to the run.
- **Releases:** merging the release-plz version PR tags `vX.Y.Z`, which builds + signs + notarizes
  the macOS app and publishes it as a GitHub release.

### Where to start

Looking for something to work on? The pinned [**WPT conformance tracker** (#40)][wpt-tracker] lists
open [web-platform-tests](https://github.com/web-platform-tests/wpt) failures sorted by difficulty —
from 🟢 *good first issue* (small, self-contained, with a dedicated test to verify) through
🔴 *high-impact* fixes that unblock thousands of subtests. Each issue has a verified root cause, the
failing test, exact subtest counts, the relevant spec section, and a one-line repro. Running the
suite locally — the real `wpt run` harness driving the engine over WebDriver — is documented in
[`docs/running-wpt.md`](docs/running-wpt.md).

**Claiming an issue:** comment `/claim` (or `.take`) on any open issue to have it assigned to you —
a bot assigns you and labels it `claimed`, no maintainer needed. Comment `/unclaim` to release it.
Then open a PR that says `Closes #<n>` so it auto-closes on merge.

[wpt-tracker]: https://github.com/lucid-softworks/browser/issues/40

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

Long pages **scroll** (mouse-wheel, per-tab, clamped to document height), and `<img>`
**images** are fetched, decoded, and blitted (sized by CSS dims or decoded intrinsic size).

Done: networking + external CSS/JS · HTML→DOM · tabs · JS (DOM bindings + timers + browser
env) · CSS cascade · box-model + flexbox/grid/positioning layout + paint · scrolling · images.
Roadmap: z-index paint order · floats · margin collapsing · fuller grid · `data:`/SVG images ·
DOM events on input · `fetch`/XHR · concurrent fetch · GPU rendering.

## Star History

<a href="https://www.star-history.com/?repos=lucid-softworks%2Fbrowser&type=timeline&legend=top-left">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/chart?repos=lucid-softworks/browser&type=timeline&theme=dark&legend=top-left" />
   <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/chart?repos=lucid-softworks/browser&type=timeline&legend=top-left" />
   <img alt="Star History Chart" src="https://api.star-history.com/chart?repos=lucid-softworks/browser&type=timeline&legend=top-left" />
 </picture>
</a>
