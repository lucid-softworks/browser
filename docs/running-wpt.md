# Running the Web Platform Tests

We run [web-platform-tests](https://github.com/web-platform-tests/wpt) the same way other browsers
do: the real **`wpt run`** harness drives our engine over **WebDriver**. Our WebDriver server lives
in [`crates/webdriver`](../crates/webdriver/README.md); a small wptrunner "product"
([`tools/wpt/lucid.py`](../tools/wpt/lucid.py)) tells `wpt run` how to launch and drive it.

> **Never edit WPT tests to make them pass — fix the engine.** The vendored tests are the spec
> oracle. (Engine *unit* tests may change freely.)

## One-time setup

```sh
# 1. A WPT checkout at ./wpt (gitignored). Blobless + sparse keeps it small.
git clone --depth 1 --filter=blob:none --sparse https://github.com/web-platform-tests/wpt.git wpt
( cd wpt && git sparse-checkout set tools third_party docs resources common )

# 2. Map the WPT subdomains to loopback (wpt serve binds them; the engine resolves them).
#    System-level + needs sudo; reversible by deleting the appended block from /etc/hosts.
( cd wpt && python3 ./wpt make-hosts-file | sudo tee -a /etc/hosts )
```

`wpt run` needs `python3`; it manages its own virtualenv under `wpt/_venv3`.

## Run

```sh
# Build the WebDriver server, install the product into the checkout, and run an area / dir / file.
scripts/run-wpt.sh fetch/api/headers
scripts/run-wpt.sh url
scripts/run-wpt.sh dom/nodes/Node-isEqualNode.html

# Pass extra `wpt run` flags after `--`, e.g. a machine-readable report and verbose logs:
scripts/run-wpt.sh fetch/api/headers -- \
  --test-types testharness --log-wptreport="$PWD/report.json" --log-mach=- --log-mach-level=info
```

`scripts/run-wpt.sh` ensures the sparse checkout has the serve infrastructure, copies the `lucid`
product into the checkout's `wptrunner.browsers` package, builds `crates/webdriver`, and runs
`wpt run lucid <tests>`.

## CI

[`.github/workflows/wpt-run.yml`](../.github/workflows/wpt-run.yml) runs the complete upstream WPT
suite nightly. It generates weighted shards from the current WPT tree, sparse-checks out each
shard, and runs them in parallel through `wpt run`; every shard uploads its raw report and writes a
pass-rate summary to the job summary.

## Known limitations

- Worker variants are supported progressively; shared-worker and worker-specific platform APIs
  still have conformance gaps, so use report data rather than assuming all variants behave alike.
- Because `wpt run` launches a session per test, large areas can be slow; CI legs are time-bounded
  and report whatever completed (an area may show as `incomplete`).
