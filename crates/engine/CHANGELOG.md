# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/lucid-softworks/browser/compare/v0.0.0...v0.2.0) - 2026-06-27

### Added

- *(perf)* Navigation Timing 2 + iframe/object navigation infrastructure ([#120](https://github.com/lucid-softworks/browser/pull/120))
- *(svg)* SVG IDL conformance — idlharness.window.html to 100% ([#119](https://github.com/lucid-softworks/browser/pull/119))
- honor the CORS credentials mode for cookies
- *(js)* follow redirects in the CORS layer with per-hop checks
- *(net)* return 4xx/5xx as responses and don't follow preflight redirects
- *(net)* expose response headers and status text to fetch/XHR
- *(cookies)* shared jar with prefix/Secure/SameSite rules and window.open contexts ([#117](https://github.com/lucid-softworks/browser/pull/117))
- *(engine)* CSS Custom Highlight API + ::highlight(name) painting
- *(engine)* paint ::selection for programmatic getSelection() highlights
- *(engine)* forced-colors backplate spans the line box (pre-pass)
- *(style)* forced color inherits + SVG/visited currentColor mapping
- *(engine)* render uniform SVG gradients as solid fills
- *(engine)* force non-system SVG gradient stop-colors in forced colors
- SVG fill/stroke currentColor follows the forced color
- background-image viewport propagation + SVG forced-color-adjust:none default
- *(engine)* SVG reads CSS fill/stroke + forces them in forced colors
- *(style)* :visited privacy — keep LinkText computed, map to VisitedText at paint
- *(engine)* viewport background uses only <html> in forced colors
- self.crossOriginIsolated from COOP+COEP response headers
- *(js)* dedicated Web Workers (per-realm) and OffscreenCanvas
- *(engine)* reconstruct WOFF2 glyf/loca/hmtx table transforms
- *(engine)* render @font-face web fonts, including WOFF2 decoding
- *(css)* clip overflow:hidden content
- *(svg)* linear/radial gradient fills
- *(svg)* nested <svg> viewports and <use> references
- *(css)* pixel background-position and background-size (CSS sprites)
- *(engine)* render direct image navigations as images
- *(css)* render background-image url() (size/repeat/position)
- *(engine)* font fallback for non-Latin glyphs
- *(engine)* decode SVG images in img tags
- *(engine)* site favicons in the tab and address bar
- *(net)* URL fixup, HSTS, and http fallback in the engine (not the shell)
- *(css)* pass all css/CSS2/positioning via scroll-clamp, text-indent, inline static position, and @font-face web fonts ([#107](https://github.com/lucid-softworks/browser/pull/107))
- *(layout)* CSS floats plus Wikipedia rendering and cascade-perf fixes ([#105](https://github.com/lucid-softworks/browser/pull/105))
- *(dom)* CharacterData methods, CDATASection, and arena-backed off-documents ([#3](https://github.com/lucid-softworks/browser/pull/3))
- *(engine)* add JPEG XL (.jxl) image decoding ([#4](https://github.com/lucid-softworks/browser/pull/4))

### Fixed

- *(style)* fully-transparent background-color is no background
- *(engine)* SVG currentColor resolves to the element color, not inherited fill/stroke
- *(js)* fire <body onload> on the window (Window-reflecting body handlers)
- *(svg)* parse packed path numbers; prefer url() background layer
- *(engine)* default to a proportional sans-serif font, not monospace
- *(engine)* scale page layout by the backing scale on HiDPI/Retina ([#88](https://github.com/lucid-softworks/browser/pull/88))

### Other

- extract the URL parser into a shared `wurl` crate; drop url from all crates
- satisfy rustfmt and clippy (-D warnings)
- *(engine)* cap concurrent image fetches at 5
- split monolithic lib.rs files into focused modules ([#59](https://github.com/lucid-softworks/browser/pull/59))
- green up the cross-platform matrix (exclude ffi on Linux/Windows; clippy 1.96) ([#2](https://github.com/lucid-softworks/browser/pull/2))
