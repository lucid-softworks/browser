# HTML element support audit

The previous element matrix predated table layout, form widget painting, SVG, generated content,
presentational attributes, dialog methods, selection, and streamed loading. Those features now have
engine tests and are no longer listed as gaps here.

## Well-supported groups

- Sectioning, headings, paragraphs, lists, quotations, preformatted text, links, and common inline
  semantics have UA styles and participate in layout/paint.
- Tables have row groups, shared columns, captions, spans, separated/collapsed borders, colgroup
  widths, and common presentational attributes.
- Common form controls render and handle focus, typing, selection, labels, toggles, and change/input
  events. Dialog, details, and summary expose their core interactive behavior.
- Raster images, data URLs, JPEG XL, inline SVG, and Canvas 2D render through the engine.

## Remaining partial or unsupported groups

- [ ] **Media:** `<video>`, `<audio>`, `<track>`, and complete `<source>` selection/playback.
- [ ] **Nested/embedded content:** rendered `<iframe>` browsing contexts, `<embed>`, and `<object>`.
- [ ] **Image maps:** `<map>` and `<area>` geometry and activation.
- [ ] **Advanced forms:** multipart file submission, constraint-validation UI, and
  form-associated custom elements through `ElementInternals`.
- [ ] **International text:** `<bdi>`/`<bdo>` bidi behavior and ruby annotation layout.
- [ ] **Popover/top layer:** rendering, focus, stacking, and dismissal behavior.

## Audit method

When updating this file, verify an element through engine layout/paint, computed style and geometry,
DOM reflection, and relevant interaction behavior. Prefer WPT coverage; add focused engine tests for
rendering behavior that WPT does not directly observe.
