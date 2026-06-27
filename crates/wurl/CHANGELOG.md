# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/lucid-softworks/browser/compare/v0.1.0...v0.2.0) - 2026-06-27

### Added

- *(wurl)* Unicode 17.0 tables (CJK Ext J) — IdnaTestV2 fully passes
- *(url)* legacy text-encoding query encoding (Encoding Standard)
- *(wurl)* IDNA CheckBidi rule (RFC 5893) + Bidi_Class/Mark tables
- *(wurl)* full UTS-46 IDNA from generated Unicode 16 tables (no idna crate)

### Fixed

- *(wurl)* reject ACE label that decodes to another xn-- label
- *(url)* empty/fragment ref resolution, port whitespace no-op, blob origin scheme

### Other

- *(url)* self-contained wurl unit tests + data: iframe test; skip-if-no-wpt
- extract the URL parser into a shared `wurl` crate; drop url from all crates
