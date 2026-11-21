# Running Speedometer 3.1

Speedometer exercises 20 web application workloads: TodoMVC frameworks, news sites,
editors, charts, and a performance dashboard. Use the upstream suite without editing
its tests or replacing unsupported workloads.

## Set up

```sh
git clone --branch release/3.1 https://github.com/WebKit/Speedometer.git target/speedometer-source
git -C target/speedometer-source checkout 1386415be8fef2f6b6bbdbe1828872471c5d802a
cargo build --release -p webdriver
```

That revision includes built application assets. The runner rejects a modified benchmark
checkout. It starts a local HTTP server and fresh WebDriver processes, runs browsers
sequentially, and cleans up the processes and server when finished.

Python 3.11+ and POSIX process groups (Linux/macOS) are required. For comparisons,
install Chrome, Firefox, and compatible
`chromedriver` and `geckodriver` binaries. Put the drivers on `PATH` and set `CHROME`
and `FIREFOX` to the browser executable paths. `CHROMEDRIVER`, `GECKODRIVER`, and
`WEBDRIVER_BIN` optionally override the driver paths.

```sh
python3 scripts/run-speedometer.py \
  --browsers chrome firefox --iterations 10 --repeats 3 \
  --output target/speedometer-reference

python3 scripts/run-speedometer.py \
  --browsers engine --iterations 1 --repeats 1 \
  --output target/speedometer-engine
```

Each output directory must be new. Stop builds, other benchmarks, and CPU-intensive
applications before recording performance results. Use the same machine and browser
configuration for every run. The runner alternates browsers between repeats; each
repeat gets a fresh session. Chrome and Firefox run headless at a requested outer
window size of 1280 × 900. Speedometer sets its application iframe to 800 × 600.

## Investigate one workload

```sh
python3 scripts/run-speedometer.py \
  --browsers engine --suite TodoMVC-jQuery --iterations 1 --repeats 1 \
  --output target/speedometer-jquery
```

`--timeout` limits a complete browser run (default 1800 seconds). Engine tasks have a
separate `--engine-task-budget` (default 180 seconds, maximum 600). This sets
`BROWSER_SCRIPT_BUDGET_MS` only in the launched driver process. Normal browsing keeps
the existing 20-second task limit. The heap allocation limit is unchanged.

The engine's WebDriver status-poll timeout allows the configured task budget plus
five seconds for the response. This prevents a long benchmark task from being mistaken
for a failed poll; the complete-run deadline still applies.

`report.json` records capabilities, completed steps, iterations, raw subtest timings,
metrics, errors, and the engine binary SHA-256. `progress.json` records the latest
completed poll. `metadata.json` records the source revision, engine revision and diff,
platform, and configuration. A run fails if the client errors, stops early, omits
steps/iterations, or produces an invalid score. Failed runs are missing measurements,
not zero-duration results.

## Interpretation

**The engine's results are diagnostic, not comparable official Speedometer scores.**
Engine runs now include iframe JavaScript, DOM, layout, rasterization, and compositing.
Child documents use the normal rendering pipeline, including nested frames and canvas,
and live sessions yield between tasks so the host can paint. Harness completion alone
does not establish pixel equivalence with Chrome and Firefox or full rendering conformance.

Compare Chrome and Firefox timings with each other. Show the engine's workload
completion separately until its rendering pipeline is equivalent. For performance
regressions within the engine, keep the workload, binary configuration, task budget,
and rendering limitations fixed, and repeat measurements under quiet conditions.

## Iframe rendering validation

Child documents now render into their own viewport and composite at the iframe's
position in the parent paint order. The renderer follows live frame navigation and
removal, nested documents, viewport changes, device scale, and child scrolling.
Canvas pixels are returned to their owning realm for readback. Cross-origin frames
paint without exposing their document through the parent DOM APIs.

Eight pixel regression tests cover those paths, live mutations, percentage widths,
element scroll reset/detachment, and painting between successive tasks. Scroll-offset
collection tracks only scrolled elements, avoiding wrapper allocation for untouched
static content. All 901 workspace tests pass (one ignored),
and formatting and workspace Clippy checks pass; the vendored Lumen dependency
continues to emit its 13 existing compiler warnings.
The WebDriver navigation regression also verifies that assigning the current hash
does not reload the page or erase application state.

The [run with iframe painting](benchmarks/speedometer-3.1-iframes-2026-11-21.json)
completed all **20 workloads and 58 steps** in one session with one iteration and
the 180-second task budget. Both the parent and child frames reported no uncaught
task errors. The record includes raw per-step timings, source revision, engine
binary hash, and a digest of the working-tree patch used for the run.
The real WebDriver WPT smoke run matched main: 282/359 passing subtests across
20 files, with zero regressions.

This change addresses rendering. Native pointer/keyboard routing into child frames
and full browser rendering conformance remain separate work.

## Recorded validation before iframe painting — 2026-11-21

The unmodified pinned suite completed all **20 workloads and 58 steps** in one engine
session, using one iteration and the 180-second diagnostic task budget. Both the parent
page and workload frames reported no uncaught task errors.

The fixes cover coroutine heap ownership, per-frame modules and host state, DOM wrapper
collection and reuse, input value semantics, missing DOM APIs, and geometry used by
responsive charts and virtualized editors. Nested flex scroll containers now retain
their assigned height, rather than expanding to the editor's entire document.

Validation: 893 workspace tests passed, one ignored; formatting passed; Clippy reported
no workspace warnings (the vendored Lumen dependency still emits its 13 existing compiler
warnings). The real WebDriver WPT smoke run matched main: 282/359 passing subtests across
20 files, with zero regressions. This does not imply full WPT conformance.

The [recorded results](benchmarks/speedometer-3.1-2026-11-21.json) include the measured
engine binary hash, browser versions, all workload samples, and reference-run scores.
These historical engine samples predate iframe painting and must not be compared
with timings from the current rendering path.
Chrome and Firefox each use three fresh sessions of ten iterations, run sequentially
without overlapping task-owned builds or benchmarks. The machine is a shared Linux host;
background services remain active, so the timings are indicative of this environment.
