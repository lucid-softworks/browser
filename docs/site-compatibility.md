# Real website compatibility

The manual smoke suite drives this browser through its WebDriver server against
four live websites. It tests useful interactions, including page navigation and
React updates. A successful install or rendered initial page alone does not count
as a passing journey.

Build and run from the repository root with Python 3 (no Python packages needed):

```sh
cargo build -p webdriver
python3 scripts/run-sites.py --output target/site-results-first-run
```

Select a subset or a different build:

```sh
python3 scripts/run-sites.py \
  --site example --site react-todomvc \
  --webdriver target/release/webdriver \
  --output target/site-results-release
```

Each run needs a new output directory, preserving previous evidence. Site IDs are
`example`, `wikipedia`, `hackernews`, and `react-todomvc`. The default output is
`target/site-results`. The runner creates a new WebDriver process and session for
each site, uses an available localhost port, and shuts down that process afterward.
There is no shared browser state between journeys. `NET_CACHE_DIR=off` avoids stale
pages, and `WPT_INSECURE_TLS=0` requests normal TLS certificate validation.

## Journeys

The selectors and expectations live in [`tools/sites/sites.json`](../tools/sites/sites.json).

| Website | Required behavior |
| --- | --- |
| [example.com](https://example.com/) | Read the page title and English explanation, follow the IANA link, read its destination heading, and return with Back. |
| [Wikipedia](https://www.wikipedia.org/) | Enter “Web browser” in portal search, click Search, read the matching English article and body, and return to the portal. |
| [Hacker News](https://news.ycombinator.com/) | Read front-page story links, open the first story’s discussion, and return to the front page. A story with zero comments is valid. |
| [React TodoMVC](https://todomvc.com/examples/react/dist/) | Add two tasks with Enter, complete the first, verify Active and Completed filters, delete the completed task with Clear completed, and confirm only the active task remains. |

The example.com page currently has a document title rather than an `h1`. Its
markup was checked against the live response when the journey was added. Website
redesigns can require selector updates; first inspect the captured page before
attributing a failed assertion to the engine.

Content assertions also require nonzero layout boxes and check for hidden or
transparent ancestors, so DOM nodes alone do not satisfy the journey.
This is not a pixel comparison: inspect
the screenshots to assess typography, clipping, images, and overall layout.

All clicks and typing use WebDriver element commands. Enter is the W3C WebDriver
key `U+E007`. JavaScript execution only reads DOM state for assertions; the runner
does not inject events, mutate application state, or substitute fixture content.
The TodoMVC deletion uses its visible Clear completed button.

## Results and limits

The process exits **0 only if every selected journey passes**; any failed step,
navigation, WebDriver error, or timeout produces exit **1**. Invalid CLI arguments
produce exit **2**. Later steps of a failed journey are recorded as `not_run`, and
the remaining sites still run. There are no expected-failure exemptions.

The output directory contains an aggregate `report.json` and one directory per
site with:

- `report.json`: step results, timings, observed DOM state, errors, and artifact availability.
- `page.html`: final page source, including after a failed step.
- `screenshot.png`: the browser’s rendered final viewport.
- `url.txt`: final browser URL.
- `driver.log`: WebDriver stdout and stderr.

Capture is attempted on success and failure. If the driver crashes or stalls,
the report explains which artifacts could not be collected. The runner does not
claim success when evidence is unavailable because a journey failed.

`--timeout` sets the per-site journey budget (default 180 seconds),
`--request-timeout` caps a WebDriver HTTP request (default 30 seconds), and `--wait`
sets assertion polling time (default 8 seconds). Cleanup allows an additional
10 seconds for diagnostics and up to 4 seconds to terminate the process. No
interactive browser window is opened; screenshots come from the browser engine.

These are live-network diagnostics: outages, rate limiting, changing site
content, and connectivity can fail a run. Keep them manual or optional scheduled
checks, separate from required offline unit tests. Use deterministic local
fixtures and engine tests to regress each compatibility fix. WPT remains the
standards conformance oracle; this suite does not replace it or edit its tests.

Passing these DOM, layout-box, and interaction checks does not mean a page is
pixel-perfect. TodoMVC's footer filters can appear near the top of the app because
absolute positioning with automatic offsets does not yet preserve their expected
static position. Screenshots remain necessary for visual review.

Navigation also has limits beyond these journeys: forms currently support GET
submissions; POST and file uploads remain unsupported. JavaScript
`history.pushState` and `history.replaceState` do not yet update native history
entries. See the [WebDriver limitations](../crates/webdriver/README.md) for details.

On **2026-11-15**, all **4/4 live journeys passed** against the local Linux
WebDriver build: example.com, Wikipedia, Hacker News, and React TodoMVC. This
verifies the interactions listed above, including React task creation, completion,
filtering, and deletion. It does not establish visual parity or equivalent live
results on macOS and Windows; those platforms still need their own runs.

This dated result is a snapshot. Inspect `report.json` from a fresh run against
the commit and platform being tested for current compatibility status.
