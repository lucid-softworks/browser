# AGENTS.md

Conventions for AI coding agents (and humans driving them) working in this repo. This file is the
quick, actionable contract; see `README.md` for architecture and the PR template for the checklist.

## Pull requests

- **The PR title MUST be a [Conventional Commit](https://www.conventionalcommits.org/).** PRs are
  **squash-merged**, so the title *becomes* the commit message and drives automatic versioning
  (release-plz). A non-conforming title fails CI (`amannn/action-semantic-pull-request`).
  - Format: `type(scope): summary` — e.g. `feat(css): support aspect-ratio`,
    `fix(url): strip tab/newline before parsing`, `ci: cache the prebuilt V8 static lib`.
  - **Allowed types:** `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`,
    `chore`, `revert`.
  - **Scope** is optional but preferred; use the crate name (`engine`, `dom`, `css`, `url`, …).
  - Keep one PR to one logical change so the squashed title describes it honestly.
- Note which model/tooling produced the PR (this project is **LLM-first** — prefer changes authored
  by a capable coding model and reviewed by a human).

## Before you open a PR

Run, from the workspace root:

```sh
cargo fmt --all
cargo clippy --workspace        # must be clean (warnings are denied in CI)
cargo test --workspace          # CI runs this on macOS / Linux / Windows
```

## Guiding constraint

The eventual goal is to **rewrite everything in Rust**, including the parts currently reused. Every
reused crate is walled off behind *our own* module boundary (`net::fetch`, `paint::GlyphRasterizer`,
`js::Runtime`, …) so swapping it for a hand-written implementation later is a localized change. When
adding a dependency, keep it behind such a boundary and prefer pure-Rust crates.

## Web Platform Tests (conformance)

- **Never edit WPT tests to make them pass — fix the engine.** The vendored tests are the spec
  oracle; changing them defeats the purpose. (Engine *unit* tests may change freely.)
- Run the suite locally the way other browsers are tested — the real `wpt run` harness driving the
  engine over WebDriver:
  ```sh
  scripts/run-wpt.sh dom/nodes        # an area, dir, or single test file
  ```
  See [`docs/running-wpt.md`](docs/running-wpt.md) for one-time setup (WPT checkout + hosts).
- CI runs the WPT conformance suite on PRs that can affect it; don't regress it.

## Layout

Engine crates live under `crates/` (`net`, `html`, `css`, `dom`, `js`, `style`, `layout`, `paint`,
`engine`, `ffi`, `webdriver`). The native app shell is under `swift/`. Keep platform-agnostic engine
code free of OS/shell concerns.
