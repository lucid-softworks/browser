# WPT conformance — biggest wins

Baseline 501/5295 (9.5%) → after implicit-body + classList: **1918/5254 (36.5%)**, 466 files ran,
34 harness-errors. Prioritized by subtests unlocked.

## Top wins (by impact)
- [x] **Implicit `<head>`/`<body>`** — `document.body === null` on bodyless pages. **29 files die outright** (`null.insertBefore`) + 38 more fail `appendChild on null`; nearly every WPT test appends fixtures to `document.body`. (`crates/html` parser) — **gates the most**
- [x] **`classList` / `DOMTokenList`** → 1420/1420 (+ MutationObserver.takeRecords fix) — `Element-classlist.html` is **20/1420**. Full DOMTokenList: add/remove/toggle(force)/replace/contains/item/length/value/supports, indexing, iteration, token validation (throw on empty/whitespace). (`crates/js`)
- [x] **Namespaces** (createElementNS/createAttribute/getElementsByTagNameNS — case.html 285/285) — `createElementNS` (596) + `Document-createElement-namespace` (51): `namespaceURI`/`prefix`/`localName`, `createElementNS`/`createAttributeNS`, `getElementsByTagNameNS`. (`crates/js`)
- [x] **`createElement` edge cases** (lowercasing, InvalidCharacterError, tagName/localName) (147) — invalid-name `InvalidCharacterError`, lowercasing, `localName`/`tagName`/`nodeName`. (`crates/js`)
- [ ] **`cloneNode`** (135) — deep/shallow real clone (attrs + children), not `return this`. (`crates/js`)
- [x] **Event constructors** — subclasses 49/49, createEvent 273/273, full dispatch — `createEvent` (273) + `Event-subclasses-constructors` (49): `new Event/CustomEvent/...`, `initEvent`, `bubbles`/`cancelable`/`composed`. (`crates/js`)
- [ ] **ChildNode/ParentNode mutation mixins** — `before`/`after`/`replaceWith`/`remove`/`prepend`/`append`/`replaceChildren` (~45 each across files); `insertBefore`/`appendChild` `HierarchyRequestError` + proper `DOMException`. (`crates/js`)
- [ ] **`textContent`** (75), **attributes** reflection (63), **`createAttribute`** (36), **CharacterData** methods (`replaceData`/`appendData`/… 34). (`crates/js`)
- [ ] **DOMException correctness** — many `assert_throws_dom`/`assert_throws_js` rely on the right error name/type being thrown.

Goal: clear 10%; these top items should push well past it.
