# WPT conformance backlog

The last recorded broad run was **63,674/69,572 (91.5%)**, before several later DOM, CSS, worker,
streaming, and rendering changes. Its per-test failure list is stale and must not be used to claim a
feature is currently absent.

## Next baseline refresh

- [ ] Run the current WPT matrix through `scripts/run-wpt.sh` or CI.
- [ ] Store each raw `wptreport` artifact and generate an aggregate report with
  `scripts/wpt-report.py`.
- [ ] Replace the provisional areas below with measured failing file/subtest counts and exact
  reproduction commands.

## Provisional high-value areas

- [ ] `webdriver/` navigation: back/forward history and invalid-session behavior.
- [ ] CSS overflow and CSSOM View: nested scrollports, element offsets, clipping, and hit testing.
- [ ] HTML popovers and top-layer behavior.
- [ ] Custom elements and `ElementInternals` form association.
- [ ] CSS animations, transitions, and Web Animations integration.
- [ ] CSS percentage geometry, resolved values, gradients, and border radii.
- [ ] Nested browsing contexts and cross-document behavior.
- [ ] Worker-specific Cookie Store and service-worker integration.

## Tracker requirements

Every conformance issue should include the failing test path, subtest count, relevant spec section,
verified root cause, and a one-line `scripts/run-wpt.sh` reproduction. Never edit WPT expectations or
test inputs to make an engine failure pass.
