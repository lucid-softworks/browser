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

## Backends

By default the runner uses a built-in **static** file server: single-origin, http-only,
dependency-free. It can't execute WPT's server-side endpoints (`record-headers.py`, `stash.py`, …),
do request-context `.sub` substitution (`{{headers[...]}}`, `{{uuid()}}`), serve multiple origins,
or speak `.https` — so areas that lean on those (`fetch/metadata`, `service-workers`) fail for infra
reasons rather than engine bugs (see issue #78).

Set `WPT_WPTSERVE=1` to drive the **real `wpt serve`** instead, which does all of the above. It
needs a one-time setup:

```sh
# 1. Add the serve infrastructure to the sparse checkout (alongside resources/common/<area>).
( cd wpt && git sparse-checkout add tools third_party docs )

# 2. Map the WPT subdomains to loopback so `wpt serve` can bind and the engine can resolve them.
#    (One-time, system-level — reversible by deleting the appended block from /etc/hosts.)
( cd wpt && python3 ./wpt make-hosts-file | sudo tee -a /etc/hosts )

# 3. Run. The runner spawns `wpt serve`, injects the result-capturing shim via --inject-script,
#    trusts the WPT CA (tools/certs/cacert.pem) for .https, and tears the server down on exit.
WPT_WPTSERVE=1 cargo run --release -p wpt-runner -- ./wpt fetch/metadata 200000
```

`wpt serve` needs `python3` on `PATH`; its log goes to `/tmp/wptserve.log`. The default (static)
backend is unaffected, so existing CI runs unchanged until the workflow is migrated.
