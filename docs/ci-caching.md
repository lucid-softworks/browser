# CI build caches

Rust CI, Desktop, WPT smoke, and the nightly WPT build use two complementary caches:

- `Swatinem/rust-cache` restores Cargo downloads and compiled dependency artifacts. Its keys
  include the runner platform, Rust compiler, build environment, and dependency manifests.
  Desktop keeps a separate key for each OS/architecture. Release CI and nightly WPT builds
  share `release-deps`. WPT smoke retains separate base/head keys because it uses a nested
  checkout and a different target path.
- `mozilla-actions/sccache-action` stores compiler results in GitHub Actions' cache. It can
  reuse unchanged workspace library compilation, which rust-cache normally removes before
  saving. Incremental compilation is disabled in these jobs because it prevents sccache hits.

Dependency caches are saved even if a later test or packaging step fails. Restoring a cache
does not skip tests, lint, packaging, signature verification, or artifact uploads. Cargo still
checks freshness, and sccache keys compiler results by their inputs.

## Checking whether caching helped

Open a workflow job's **Rust Cache** step for restore keys and hit/miss messages. Its post step
shows whether new dependency artifacts were saved. The **Post Sccache Action** step reports
cache hits, misses, and non-cacheable compiler calls.

Expect a cold build for a new platform, toolchain, or dependency set. GitHub scopes caches by
branch: PRs can restore the default branch's cache, but their new entries are scoped to the PR.
Successful main builds therefore warm caches for later PRs. Cache eviction can cause misses
again. The first run after a cache configuration change may also be cold.

Final executable/test linking, including release LTO, still takes time; sccache does not cache
Rust binary linking. Tests and packaging also run on every build. Compare timings and cache
statistics on subsequent runs before attributing a slow job to a cache miss.

References: [rust-cache inputs](https://github.com/Swatinem/rust-cache#usage),
[sccache setup](https://github.com/mozilla-actions/sccache-action#usage), and
[GitHub cache access](https://docs.github.com/en/actions/using-workflows/caching-dependencies-to-speed-up-workflows#restrictions-for-accessing-a-cache).
