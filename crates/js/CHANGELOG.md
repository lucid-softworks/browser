# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/lucid-softworks/browser/compare/v0.1.0...v0.2.0) - 2026-07-01

### Added

- *(js)* from-scratch JS engine (lumen) + V8/lumen backend switch + test262 loop
- *(cookies)* Cookie Store API + assorted WPT fixes (cookiestore 0 → 70/74) ([#123](https://github.com/lucid-softworks/browser/pull/123))
- *(perf)* Navigation Timing 2 + iframe/object navigation infrastructure ([#120](https://github.com/lucid-softworks/browser/pull/120))
- *(svg)* SVG IDL conformance — idlharness.window.html to 100% ([#119](https://github.com/lucid-softworks/browser/pull/119))
- *(js)* client-hint safelisting + loop-clock preflight-cache TTL
- *(js)* add XMLHttpRequest.upload EventTarget
- honor the CORS credentials mode for cookies
- *(js)* follow redirects in the CORS layer with per-hop checks
- *(js)* CORS-preflight result cache
- *(js)* precise CORS request-header safelisting and expose-header parsing
- *(js)* implement CORS for XMLHttpRequest and fetch
- *(net)* expose response headers and status text to fetch/XHR
- *(cookies)* shared jar with prefix/Secure/SameSite rules and window.open contexts ([#117](https://github.com/lucid-softworks/browser/pull/117))
- *(engine)* CSS Custom Highlight API + ::highlight(name) painting
- *(engine)* paint ::selection for programmatic getSelection() highlights
- *(style)* computedStyleMap reports computed (pre-forced) colors + extra color props
- *(js)* minimal Element.computedStyleMap() (CSS Typed OM)
- *(js)* decode data: URLs in the iframe loader
- *(url)* legacy text-encoding query encoding (Encoding Standard)
- *(js)* from-scratch WHATWG URL parser; drop the url and idna crates
- *(js)* iframe src navigation + contentDocument exposes the loaded realm
- *(js)* DOMException legacy code constants + branding-checked attributes
- *(js)* make URL & URLSearchParams proper WebIDL interfaces
- *(js)* execute javascript: URLs on <a>/<area> activation
- *(js)* srcless iframes get an about:blank realm; contentWindow.location throws
- *(js)* parse URLs in Rust via the url crate
- *(js)* URLSearchParams + URL conformance fixes
- *(js)* real iframe browsing contexts (nested realms)
- *(js)* Performance as a real interface; defClass IDL conformance
- *(js)* DocumentTimeline + frame-time rAF, window.viewport segments
- self.crossOriginIsolated from COOP+COEP response headers
- *(js)* route async fetch/WebSocket inside dedicated workers
- *(js)* performance EventTarget + toJSON, inline data:/blob: workers
- *(js)* real performance.timeOrigin/now() + crossOriginIsolated
- *(js)* dedicated Web Workers (per-realm) and OffscreenCanvas
- *(js)* ParentNode insertion, form-control, media, SVG-anim, storage stubs
- *(js)* give iframe contentWindow the common window facade members
- *(js)* implement Range.extractContents / deleteContents
- *(js)* Selection caret API (collapse/extend/selectAllChildren/…)
- *(js)* stub TextEvent.initTextEvent, attachInternals, Animation.commitStyles
- *(js)* legacy Event init* methods + document getRootNode/isSameNode
- *(js)* fill in DOM/crypto APIs flagged "is not a function" by WPT
- *(js)* Web Crypto AES-CBC / AES-CTR encrypt+decrypt
- *(js)* Web Crypto subtle digest + HMAC
- *(js)* in-memory IndexedDB
- *(js)* custom element disconnected/attributeChanged lifecycle callbacks
- *(js)* implement a real structuredClone
- *(js)* Resource Timing + PerformanceObserver, with CORS-aware CSS subresource fetching
- *(js)* add minimal Element.animate (Web Animations lifecycle)
- *(webdriver)* implement pointer Actions and fix testdriver input round-trip
- *(js)* implement window.postMessage (same-window delivery)
- *(css)* pass all css/CSS2/positioning via scroll-clamp, text-indent, inline static position, and @font-face web fonts ([#107](https://github.com/lucid-softworks/browser/pull/107))
- *(dom)* implement ARIA element reflection (aria*Element/aria*Elements) ([#68](https://github.com/lucid-softworks/browser/pull/68))
- *(dom)* implement Range.prototype.cloneContents ([#65](https://github.com/lucid-softworks/browser/pull/65))
- *(dom)* implement Selection API and live Range mutations ([#62](https://github.com/lucid-softworks/browser/pull/62))
- *(dom)* implement XMLDocument for createDocument and fix QName validation ([#61](https://github.com/lucid-softworks/browser/pull/61))
- *(sw)* implement the Service Worker API ([#56](https://github.com/lucid-softworks/browser/pull/56)) ([#57](https://github.com/lucid-softworks/browser/pull/57))
- *(fetch)* support FormData bodies and fix Request URL percent-encoding ([#54](https://github.com/lucid-softworks/browser/pull/54))
- *(dom)* implement innerText/outerText getter and setters ([#50](https://github.com/lucid-softworks/browser/pull/50))
- *(dom)* CharacterData methods, CDATASection, and arena-backed off-documents ([#3](https://github.com/lucid-softworks/browser/pull/3))

### Fixed

- *(html/dom)* implement document named properties ([#125](https://github.com/lucid-softworks/browser/pull/125))
- *(js)* structuredClone a cross-realm plain object
- *(js)* redirect method/body/redirected-flag correctness
- *(js)* fire load on <link rel=preload> so reftest-wait clears
- *(url)* empty/fragment ref resolution, port whitespace no-op, blob origin scheme
- *(js)* normalize file: drive-letter X| to X: for absolute file URLs
- *(js)* pathname setter is a no-op for an opaque-path URL
- *(js)* collapse 3+ leading slashes when resolving against a special base
- *(js)* encode opaque-path trailing space at serialization; WorkerLocation
- *(js)* window.open() throws SyntaxError on an invalid URL
- *(js)* URLSearchParams coerces strings to USVString; record init semantics
- *(js)* hyperlink protocol getter returns ":" for an unparseable URL
- *(js)* reject port-bearing host on file URLs; split host:port via IPv6-aware
- *(js)* keep opaque-path trailing spaces when removing the query
- *(js)* sendBeacon URL validation, port leading-digit parse
- *(js)* URL setter stripping/no-op, empty frag, form-decode UTF-8, XHR.open
- *(js)* live URLSearchParams iterators
- *(js)* URLSearchParams set/search-setter + lenient form decode
- *(js)* DOMParser text/html parses into an independent document
- *(js)* correct TextEncoder.encodeInto and harden the UTF-8 TextDecoder
- *(js)* fire <body onload> on the window (Window-reflecting body handlers)
- *(css)* bound grid track expansion and de-quadratic-ify CSS value parsing
- *(css)* reject invalid font-family lists and serialize escapes idempotently
- *(js)* don't call matches() on non-element ancestors in closest()
- *(dom)* add DocumentFragment getElementById ([#83](https://github.com/lucid-softworks/browser/pull/83))
- *(url)* coerce reflected URLs to USVString ([#84](https://github.com/lucid-softworks/browser/pull/84))
- *(html)* reflect hyperlink username and password ([#82](https://github.com/lucid-softworks/browser/pull/82))
- *(dom)* make Document.body live and settable ([#81](https://github.com/lucid-softworks/browser/pull/81))
- *(dom)* add WebKit event handler aliases ([#79](https://github.com/lucid-softworks/browser/pull/79))
- *(dom)* initialize StaticRange constructor ([#71](https://github.com/lucid-softworks/browser/pull/71))
- *(url)* don't add spurious // authority to non-special-scheme URLs ([#69](https://github.com/lucid-softworks/browser/pull/69))
- *(dom)* initialize Text and Comment constructors ([#67](https://github.com/lucid-softworks/browser/pull/67))
- *(dom)* make NodeIterator/TreeWalker spec-compliant ([#63](https://github.com/lucid-softworks/browser/pull/63))
- *(dom)* throw required DOMExceptions for Range/Node operations ([#53](https://github.com/lucid-softworks/browser/pull/53))
- *(js)* implement live DOM collections ([#52](https://github.com/lucid-softworks/browser/pull/52))
- *(js)* expose createRange on all documents ([#49](https://github.com/lucid-softworks/browser/pull/49))
- *(js)* scope Node.contains to its tree ([#48](https://github.com/lucid-softworks/browser/pull/48))
- *(js)* enforce dispatchEvent contract ([#45](https://github.com/lucid-softworks/browser/pull/45))
- *(dom)* implement ParentNode.childElementCount ([#43](https://github.com/lucid-softworks/browser/pull/43))
- *(js)* canonicalize DOM node wrappers so identity is stable ([#12](https://github.com/lucid-softworks/browser/pull/12))
- *(js)* stable node identity for arena documents and doctype ([#11](https://github.com/lucid-softworks/browser/pull/11))

### Other

- Merge branch 'main' into feat/lumen-js-engine
- *(lumen)* rustfmt the crate to satisfy the CI fmt gate
- *(url)* self-contained wurl unit tests + data: iframe test; skip-if-no-wpt
- extract the URL parser into a shared `wurl` crate; drop url from all crates
- cargo fmt
- split monolithic lib.rs files into focused modules ([#59](https://github.com/lucid-softworks/browser/pull/59))
