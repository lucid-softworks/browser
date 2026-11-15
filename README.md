# browser

A web browser written from scratch.

- **Rust** is "the inners": the actual engine — networking, parsing, DOM, style,
  layout, and paint. The engine is **platform-agnostic** and produces an RGBA framebuffer.
- The **desktop app** (window, URL bar, tabs, event loop) is a shared **Rust / Rune** shell
  for **Windows, Linux, and macOS**. The original **Swift / AppKit** shell remains available
  on macOS. Both use the same Rust engine and Lumen JavaScript runtime. The shared desktop
  app bundles Inter so page text works without a standard system-font installation.

## Guiding constraint

The **eventual goal is to rewrite everything in Rust**, including the parts we currently
reuse. So every reused crate is walled off behind *our own* module boundary, and swapping
it for a hand-written implementation later is a localized change, not a refactor:

| Reused today | Hidden behind | Eventually replaced by |
|---|---|---|
| `ureq` (HTTP/TLS) | `net::fetch` → `net::Response` | hand-written HTTP/1.1 (TLS likely stays reused — DIY TLS is unsafe) |
| `fontdue` (glyph raster) | `paint::GlyphRasterizer` trait | hand-written rasterizer |
| `lumen` (our JS VM) | `js::Runtime` / `js::Session` | maintained in the Lumen repository |
| `cbindgen` (header gen) | build script only | n/a (build tooling) |
| `rune` (desktop UI) | `crates/desktop` | our own native shell |

Everything else — HTML tokenizer/tree-builder, CSS parser, DOM, cascade, layout, the
compositor — is ours.

## Build & run

### Prerequisites

- **Rust 1.99.1 or newer** for the shared desktop app — install via [rustup](https://rustup.rs).
- **Windows:** Rust MSVC toolchain and Visual Studio C++ build tools.
- **Linux:** Wayland/XKB development libraries and a Vulkan driver; see [desktop setup](docs/desktop.md#linux).
- **macOS:** Xcode command-line tools (`xcode-select --install`).

Enable the versioned git hooks (a `pre-push` hook that runs the same `rustfmt` +
`clippy` gate as CI, so lint never fails the PR after the fact):

```sh
git config core.hooksPath .githooks   # one-time, per clone
```

### Engine / tests only

The engine is plain Rust and builds on **macOS, Linux, and Windows**:

```sh
cargo build --workspace --exclude browser-desktop
cargo test --workspace --exclude browser-desktop
```

Page scripts, DOM bindings, modules, workers, and iframe realms use our
[Lumen JavaScript engine](https://github.com/lucid-softworks/lumen), pinned in `Cargo.lock`.
No V8 library is needed. Run the browser binding tests with `cargo test -p js --locked`.

### Desktop app (Windows, Linux, macOS)

```sh
cargo run --locked -p browser-desktop -- --url https://example.com
cargo build --locked --release -p browser-desktop
```

Launch `target/release/browser` (`browser.exe` on Windows), or use
`scripts/run-browser.sh` / `scripts/run-browser.ps1`. The shared app uses Rune for native
windowing and UI; page content is rendered by our own engine. See
[desktop setup, headless tooling, and platform details](docs/desktop.md).

### Optional Swift/AppKit shell (macOS)

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
URL ─▶ net ─▶ HTML / DOM ─▶ style ─▶ layout ─▶ paint ─▶ RGBA
                                                       ├─▶ desktop (Rune: Windows / Linux / macOS)
                                                       └─▶ ffi (C ABI) ─▶ Swift / AppKit (macOS)

```

Rust paints an RGBA framebuffer. The shared desktop shell presents it through Rune; its
page worker owns the engine while the UI remains responsive. The optional Swift shell
uploads the same framebuffer to an `NSView` via the C ABI. OS and toolkit code stays outside
the platform-agnostic engine.

### Rust workspace (`crates/`)
- `net` — fetch a URL → bytes + content-type *(reuses `ureq`)*
- `html` — hand-written HTML tokenizer + tree builder → DOM
- `dom` — arena-based node tree
- `js` — JS runtime + DOM/`window`/`self` bindings; runs page scripts. Uses Lumen
  through an internal embedding boundary
- `css` — hand-written CSS parser (`<style>` blocks + inline `style=""`)
- `style` — cascade (UA + author + inline) → computed styles, box + flex/grid/position props
- `layout` — block/inline/inline-block, **flexbox**, basic **grid**, and **positioning**
  (relative/absolute/fixed) → positioned box tree (`TextMeasurer` decouples fonts)
- `paint` — RGBA framebuffer + fill/text primitives; `GlyphRasterizer` trait
- `engine` — orchestrates the pipeline; produces the framebuffer
- `ffi` — C ABI (`staticlib`); `cbindgen` generates `include/browser.h`
- `desktop` — shared Rune desktop UI and page-worker boundary (`browser-desktop` package)

### Swift app (`swift/`)
- `CBrowser` — system-library target wrapping `include/browser.h`
- `Browser` — AppKit app linking `libbrowser_ffi.a`

## Memory model

Each tab owns a Lumen engine on its JavaScript thread within the browser process.
The current Lumen embedding API does not expose heap-byte accounting, heap limits, or
an execution-interruption hook. Heap telemetry reports 0 (unavailable). Event-loop budgets
bound work between callbacks, but cannot interrupt a script stuck inside one callback.
The DOM arena uses `usize`; the network body backstop is 16 GiB.

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

[**`lumen`**](https://github.com/lucid-softworks/lumen) is a from-scratch
JS engine (std-only) used by the browser, grown against **tc39/test262** — it
**passes the full suite: 53,376/53,376 (100%)**, including annexB, intl402, and staging. That
covers the language core plus the built-ins — closures, classes, generators & `async`/`await`
(real suspension via stackful coroutines), ES modules (top-level await, `import defer`, source
phase), `Proxy`/`Reflect`, `RegExp` (incl. `\p{…}` and inline modifiers), typed arrays,
`Promise`, `Intl`, and `Temporal`. It now lives in its own repo (extracted with full history,
along with its test262 runner and benchmark tooling) and is consumed here as a git dependency.

Done: networking + external CSS/JS · HTML→DOM · tabs · JS (DOM bindings + timers + browser
env) · from-scratch JS engine (`lumen`, 100% test262) · CSS cascade · box-model +
flexbox/grid/positioning layout + paint · scrolling · images.
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
