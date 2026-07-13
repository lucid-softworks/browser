# CSS feature backlog

The engine already supports selector combinators, common structural/state/functional pseudo
classes, attribute selectors, generated `::before`/`::after` content, transforms, gradients,
background images, box shadows, basic grid, and writing modes. This file lists remaining or
materially partial behavior.

## P0 — layout and geometry

- [ ] **Nested overflow and scroll containers** — clipping, scrollbars, scroll offsets, hit testing,
  and element scrolling APIs.
- [ ] **`position: sticky`** — sticky constraint rectangles inside the nearest scroll container.
- [ ] **Percentage geometry** — resolve percentage border radii and percentage-bearing lengths
  against the correct containing dimensions instead of dropping or approximating them.
- [ ] **Full Grid** — implicit tracks, auto-placement edge cases, `minmax()`, intrinsic sizing,
  named lines/areas, subgrid, and complete alignment.
- [ ] **Fragmentation and multicolumn** — complete column balancing, breaks, and spanning.

## P1 — selectors and generated content

- [ ] Complete `:has()` relative-selector matching and invalidation.
- [ ] Complete namespace/default-namespace selector behavior.
- [ ] Add remaining pseudo-elements, including `::placeholder`, `::marker`, selection/highlight,
  and view-transition pseudo-elements.
- [ ] Fill WPT edge cases for forgiving selector lists, escaping, and dynamic invalidation.

## P1 — paint and effects

- [ ] **Animations and transitions** — property interpolation, timelines, `@keyframes`, events,
  cancellation, and integration with `getAnimations()`.
- [ ] **Filters** — `filter` and `backdrop-filter` pipelines.
- [ ] **Clipping/masking** — complete `clip-path`, CSS/SVG masks, mask composition, and geometry.
- [ ] **Backgrounds and borders** — multiple backgrounds, conic gradients, `border-image`, and the
  remaining repeat/position/size edge cases.
- [ ] **Shadows and outlines** — text shadows, complete inset/spread behavior, and outline geometry.
- [ ] Resolve gradient length stops from the actual gradient line rather than an assumed size.

## P1 — typography

- [ ] **Web fonts** — complete `@font-face` loading, matching, fallback, and font-display behavior.
- [ ] **Line breaking** — complete `white-space`, `word-break`, `overflow-wrap`, hyphenation, and
  `text-overflow: ellipsis`.
- [ ] **Bidi and vertical text** — Unicode bidi, logical ordering, glyph orientation, and remaining
  writing-mode interactions.
- [ ] Font shaping, kerning, ligatures, variable fonts, and language-sensitive fallback.

## P2 — cascade and newer CSS

- [ ] Complete custom-property token preservation, registered properties, cycles, and fallback
  edge cases.
- [ ] Resolve `calc()`/`min()`/`max()`/`clamp()` across all applicable value types and percentage
  bases.
- [ ] Complete cascade layers, scopes, nesting specificity, container queries, and style queries.
- [ ] Add logical shorthand coverage and remaining CSSOM serialization/resolved-value behavior.
