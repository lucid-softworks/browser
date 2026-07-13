# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/lucid-softworks/browser/compare/v0.1.0...v0.2.0) - 2026-07-13

### Added

- *(css)* support contain intrinsic sizing
- *(css)* support the overlay property
- *(engine)* paint ::selection for programmatic getSelection() highlights
- *(style)* forced color inherits + SVG/visited currentColor mapping
- *(engine)* force non-system SVG gradient stop-colors in forced colors
- *(style)* forced colors maps <mark> to Mark/MarkText + currentColor backgrounds
- SVG fill/stroke currentColor follows the forced color
- *(style)* inherited color follows forced ancestor under forced-color-adjust:none
- background-image viewport propagation + SVG forced-color-adjust:none default
- *(style)* :visited privacy — keep LinkText computed, map to VisitedText at paint
- *(style)* match :visited for same-page links
- *(style)* forced colors uses VisitedText for visited links
- *(style)* preserve background alpha in forced colors (RGB->Canvas)
- *(style)* no forced-colors backplate behind visibility:hidden/collapse text
- *(style)* preserve author system colors in forced colors mode
- *(style)* computedStyleMap reports computed (pre-forced) colors + extra color props
- *(style)* forced colors computes accent-color to auto
- *(style)* keep url() backgrounds without text in forced colors
- *(style)* drop background images in forced colors (except root/body)
- *(style)* forced colors resolves color-scheme to 'light dark'
- *(style)* forced colors computes scrollbar-color:auto, font-variant-emoji:text
- *(engine)* viewport background uses only <html> in forced colors
- *(style)* force ::before/::after pseudo-element colors in forced colors
- *(style)* real forced colors mode — backplate, link/border mapping, gated activation
- *(style)* forced colors mode — system colors + cascade override
- *(style,layout)* grid-template shorthand + grid flex baseline
- *(style,layout)* table flex baseline, caption-side, dispatch (table-001 green)
- *(style,layout)* multi-column layout (multicol-001 green)
- *(style,layout)* -webkit-line-clamp last baseline from the Nth line (line-clamp-001 green)
- *(style,layout)* logical sizes + scroll-container baseline clamp (overflow-001 green)
- *(style)* parse logical margin/padding longhands (margin-block-start etc.)
- *(style,layout)* resolve em in width/height against the element font-size; flex baseline alignment
- *(css)* support box-sizing: border-box
- *(css)* resolve rem against the root font-size
- *(css)* pixel background-position and background-size (CSS sprites)
- *(css)* render background-image url() (size/repeat/position)
- *(css)* pass all css/CSS2/positioning via scroll-clamp, text-indent, inline static position, and @font-face web fonts ([#107](https://github.com/lucid-softworks/browser/pull/107))
- *(layout)* CSS floats plus Wikipedia rendering and cascade-perf fixes ([#105](https://github.com/lucid-softworks/browser/pull/105))
- *(dom)* implement innerText/outerText getter and setters ([#50](https://github.com/lucid-softworks/browser/pull/50))

### Fixed

- *(css)* serialize resolved grid track sizes
- *(layout)* derive sizes from aspect ratios
- *(css)* canonicalize text property values
- *(css)* canonicalize flex computed values
- *(css)* preserve content alignment computed values
- *(css)* preserve self alignment computed values
- *(css)* preserve item alignment computed values
- *(css)* resolve percentage and gradient geometry
- *(style)* satisfy strict clippy gate
- *(layout)* use grid-area containing blocks for abspos grid children ([#115](https://github.com/lucid-softworks/browser/pull/115))
- *(style)* fully-transparent background-color is no background
- *(css)* bound grid track expansion and de-quadratic-ify CSS value parsing
- *(css)* reject invalid font-family lists and serialize escapes idempotently
- *(layout)* weighted flex-shrink + percentage flex-basis
- *(svg)* parse packed path numbers; prefer url() background layer

### Other

- *(style)* forced-colors override maps author colors to system colors
- Reapply "feat(style): forced colors mode — system colors + cascade override"
- Revert "feat(style): forced colors mode — system colors + cascade override"
- extract the URL parser into a shared `wurl` crate; drop url from all crates
- satisfy rustfmt and clippy (-D warnings)
- run cargo test on PRs (Linux only) ([#86](https://github.com/lucid-softworks/browser/pull/86))
- split monolithic lib.rs files into focused modules ([#59](https://github.com/lucid-softworks/browser/pull/59))
