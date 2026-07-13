# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/lucid-softworks/browser/compare/v0.0.1...v0.2.0) - 2026-07-13

### Added

- *(css)* support contain intrinsic sizing
- *(svg)* SVG IDL conformance — idlharness.window.html to 100% ([#119](https://github.com/lucid-softworks/browser/pull/119))
- *(engine)* forced-colors backplate spans the line box (pre-pass)
- *(style)* :visited privacy — keep LinkText computed, map to VisitedText at paint
- *(layout)* nested-flex-wrap baselines — line-aware descent + inline-block cross height
- *(layout)* grid cross-axis align-items (incl. baseline) — grid-001 green
- *(style,layout)* grid-template shorthand + grid flex baseline
- *(style,layout)* table flex baseline, caption-side, dispatch (table-001 green)
- *(style,layout)* multi-column layout (multicol-001 green)
- *(style,layout)* -webkit-line-clamp last baseline from the Nth line (line-clamp-001 green)
- *(layout)* central baseline for parallel vertical flex items (006/007 green)
- *(layout)* writing-mode-aware flex main axis
- *(layout)* orthogonal-flow flex baseline + vertical item cross-sizing (005 green)
- *(style,layout)* logical sizes + scroll-container baseline clamp (overflow-001 green)
- *(layout)* fieldset legend rendering (first green flex-baseline file)
- *(layout)* abspos flex children take their static position from justify-content/align-self
- *(layout)* vertical writing-mode box geometry (stage 1)
- *(style,layout)* resolve em in width/height against the element font-size; flex baseline alignment
- *(css)* clip overflow:hidden content
- *(css)* support box-sizing: border-box
- *(css)* render background-image url() (size/repeat/position)
- *(css)* pass all css/CSS2/positioning via scroll-clamp, text-indent, inline static position, and @font-face web fonts ([#107](https://github.com/lucid-softworks/browser/pull/107))
- *(layout)* CSS floats plus Wikipedia rendering and cascade-perf fixes ([#105](https://github.com/lucid-softworks/browser/pull/105))

### Fixed

- *(css)* serialize resolved grid track sizes
- *(layout)* collapse adjoining block margins
- *(layout)* apply sticky position constraints
- *(layout)* derive sizes from aspect ratios
- *(layout)* honor table width contributions
- *(css)* resolve percentage and gradient geometry
- *(layout)* use grid-area containing blocks for abspos grid children ([#115](https://github.com/lucid-softworks/browser/pull/115))
- *(layout)* <br> between block siblings creates a line box
- *(layout)* resolve percentage height + fill width:100%/height-constrained tables
- *(layout)* count inline-block atomics in intrinsic width
- *(layout)* exclude table captions from a table's flex baseline
- *(layout)* cross size of vertical-container flex items from laid-out width
- *(layout)* weighted flex-shrink + percentage flex-basis
- *(layout)* resolve explicit/percentage width on inline-blocks
- *(layout)* flow inline-level content beside floats

### Other

- *(layout)* guard vertical grid termination
- satisfy rustfmt and clippy (-D warnings)
- run cargo test on PRs (Linux only) ([#86](https://github.com/lucid-softworks/browser/pull/86))
- split monolithic lib.rs files into focused modules ([#59](https://github.com/lucid-softworks/browser/pull/59))
