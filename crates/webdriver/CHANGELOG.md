# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/lucid-softworks/browser/compare/v0.1.0...v0.2.0) - 2026-07-01

### Added

- *(cookies)* Cookie Store API + assorted WPT fixes (cookiestore 0 → 70/74) ([#123](https://github.com/lucid-softworks/browser/pull/123))
- *(cookies)* shared jar with prefix/Secure/SameSite rules and window.open contexts ([#117](https://github.com/lucid-softworks/browser/pull/117))
- *(webdriver)* implement pointer Actions and fix testdriver input round-trip
- *(wpt)* run conformance via real wpt serve + WebDriver (like other browsers) ([#80](https://github.com/lucid-softworks/browser/pull/80))

### Other

- *(webdriver)* stop polling readyState for 20s on failed navigations; resumable report runs
