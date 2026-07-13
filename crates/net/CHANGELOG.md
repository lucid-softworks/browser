# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/lucid-softworks/browser/compare/v0.0.2...v0.2.0) - 2026-07-13

### Added

- *(cookies)* Cookie Store API + assorted WPT fixes (cookiestore 0 → 70/74) ([#123](https://github.com/lucid-softworks/browser/pull/123))
- honor the CORS credentials mode for cookies
- *(net)* return 4xx/5xx as responses and don't follow preflight redirects
- *(net)* expose response headers and status text to fetch/XHR
- *(cookies)* shared jar with prefix/Secure/SameSite rules and window.open contexts ([#117](https://github.com/lucid-softworks/browser/pull/117))
- self.crossOriginIsolated from COOP+COEP response headers
- *(engine)* site favicons in the tab and address bar
- *(net)* URL fixup, HSTS, and http fallback in the engine (not the shell)
- *(wpt)* run conformance via real wpt serve + WebDriver (like other browsers) ([#80](https://github.com/lucid-softworks/browser/pull/80))

### Fixed

- *(net)* don't let default Accept/Accept-Language shadow caller headers
- *(net)* preserve post-redirect final_url across disk-cache hits ([#109](https://github.com/lucid-softworks/browser/pull/109))
- *(net)* report the post-redirect URL as final_url

### Other

- cargo fmt
- extract the URL parser into a shared `wurl` crate; drop url from all crates
- green up the cross-platform matrix (exclude ffi on Linux/Windows; clippy 1.96) ([#2](https://github.com/lucid-softworks/browser/pull/2))
