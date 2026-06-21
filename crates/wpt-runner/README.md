# wpt-runner

Runs [Web Platform Tests](https://github.com/web-platform-tests/wpt) `testharness.js` tests
against our engine **in-process** (no WebDriver). A tiny static server hosts a WPT checkout (so
`/resources/...` resolves) with our own `testharnessreport.js` injected to disable DOM output and
stash structured results on `window`; the engine loads each test, ticks the event loop until the
harness completes, and we tally subtest pass/fail.

## Usage
```
# get a WPT subset — `common` holds shared helpers many tests load via `/common/...`
# (subset-tests*.js, get-host-info, utils.js, …); omit it and those tests fail spuriously.
git clone --depth 1 --filter=blob:none --sparse https://github.com/web-platform-tests/wpt.git
cd wpt && git sparse-checkout set resources common dom/nodes

# run
cargo run --release -p wpt-runner -- <wpt-root> <subpath> [max-tests]
cargo run --release -p wpt-runner -- ./wpt dom/nodes 100
```
Reftests (no `testharness.js`) are skipped — those need the WebDriver + reftest path.
