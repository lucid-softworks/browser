# Feature backlog (non-CSS)

This is the current implementation backlog. Completed work belongs in git history and the
README status section, not in this file. See `docs/CSS-TODO.md` for CSS-specific work and
`docs/HTML-SUPPORT.md` for the element audit.

## P0 — conformance and core browser behavior

- [ ] **Form submit navigation** — perform GET/POST navigation after an uncancelled submit.
- [ ] **File upload** — multipart/form-data plus complete `File`/`Blob` integration.
- [ ] **Real iframe browsing contexts** — load and render nested documents with navigation and
  origin boundaries. The current JS window representation is not a rendered nested context.

## P1 — partial platform behavior

- [ ] **Popover/top layer** — implement visibility state, events, focus, stacking, and rendering.
- [ ] **ElementInternals** — implement form-associated custom elements, validity, labels, states,
  and form values.
- [ ] **View Transitions** — capture old/new states and render transitions; the callback/promise
  surface currently completes without a visual transition.
- [ ] **Editing commands** — implement the still-relevant `execCommand` behavior or deliberately
  scope and document the unsupported subset.

## P1 — layout, text, and media

- [ ] **Proportional and serif fonts** — select faces from `font-family`; avoid rendering all text
  with the monospaced fallback.
- [ ] **Bidi and ruby** — `direction`, `bdo`/`bdi`, Unicode bidi layout, and ruby annotation.
- [ ] **Media playback** — real `<video>` and `<audio>` loading, controls, timing, and playback.
- [ ] **Embedded content** — render `<embed>` and `<object>` resources.
- [ ] **Image maps** — hit-test `<map>`/`<area>` regions.
- [ ] **WebGL** — context creation and rendering.

## P2 — networking and browser UX

- [ ] **HTTP cache** — implement RFC cache freshness, validation, `Vary`, and invalidation. The
  current optional disk cache is GET-only and is not a complete browser cache.
- [ ] **Download handling** — content-disposition, destination selection, progress, and errors.
- [ ] **Find in page, zoom, and bookmarks**.
- [ ] **GPU compositor** — retain the platform-independent rendering boundary while replacing
  full-frame CPU uploads.

## Incremental loading follow-ups

Streaming HTML input and progressive frames exist. Remaining work is to make scheduling and
render-blocking behavior match the platform:

- [ ] Interleave parser-blocking scripts, stylesheets, images, layout, and paint according to
  `async`/`defer` and render-blocking rules.
- [ ] Avoid full re-layout/repaint when a streamed resource only invalidates a subtree.
- [ ] Add cancellation and prioritization for navigations and subresources.

## Verification debt

- [ ] Refresh the broad WPT baseline and regenerate `docs/WPT-TODO.md` from current reports.
- [ ] Add targeted WPT jobs for nested scrolling, WebDriver navigation, popovers,
  ElementInternals, and CSS percentage geometry as those features land.
