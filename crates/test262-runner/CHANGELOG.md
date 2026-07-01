# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/lucid-softworks/browser/compare/v0.1.0...v0.2.0) - 2026-07-01

### Added

- *(lumen)* box Symbol/BigInt primitives + Iterator.concat return()
- *(lumen)* SuppressedError constructor
- *(lumen)* module loader for dynamic import() in scripts/async tests + import_base
- *(lumen)* object-backed global Environment Record + async test support + print
- *(lumen)* module runner integration + top-level await + import attributes
- *(lumen)* generators + async/await (eager) + runner allocation guard
- *(js)* from-scratch JS engine (lumen) + V8/lumen backend switch + test262 loop

### Fixed

- *(test262-runner)* per-test timeout (reset deadline on worker progress, not per-chunk)
- *(test262-runner)* skip async-flagged module tests ($DONE not observed)

### Other

- *(lumen)* satisfy rustfmt + clippy lint gate
- *(test262-runner)* drop the temporary T262_GREP/T262_CAP sampling filter
- *(lumen)* rustfmt the crate to satisfy the CI fmt gate
- *(runner)* 6s chunk timeout (GC keeps memory-heavy tests fast)
- *(runner)* tune timeout/chunk/yield caps for the generator-era suite
