#!/usr/bin/env bash
#
# Maintain the WPT CSS report (wpt-report.html) from a saved wptreport JSON (_wpt_css.json) WITHOUT
# re-running the whole suite each time.
#
#   scripts/wpt-report-update.sh css/css-grid/foo.html [more tests/dirs...]
#       Re-run just those test(s)/dir(s), splice their fresh results into _wpt_css.json (matching by
#       test name; new tests are appended), and rebuild wpt-report.html.
#
#   scripts/wpt-report-update.sh --full
#       Re-run the ENTIRE css/ area (parallel), OVERWRITE _wpt_css.json, and rebuild the report.
#       This is the slow path (tens of minutes); the per-test form above is what you want day to day.
#
# _wpt_css.json is the source of truth (gitignored); wpt-report.html is regenerated from it.
# Uses $WEBDRIVER_BIN if set/executable, else falls back to target/release/webdriver (so it won't
# rebuild the engine on every call); run-wpt.sh builds one if neither exists.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
JSON="$ROOT/_wpt_css.json"
HTML="$ROOT/wpt-report.html"
PROCS="${WPT_PROCESSES:-8}"

if [ -z "${WEBDRIVER_BIN:-}" ] && [ -x "$ROOT/target/release/webdriver" ]; then
  export WEBDRIVER_BIN="$ROOT/target/release/webdriver"
fi

if [ "${1:-}" = "--full" ]; then
  echo "running the full css suite (this takes a while)…" >&2
  "$ROOT/scripts/run-wpt.sh" css -- --processes "$PROCS" --log-wptreport="$JSON"
  python3 "$ROOT/scripts/wpt-report.py" "$JSON" "$HTML" css
  exit 0
fi

if [ "$#" -eq 0 ]; then
  echo "usage: $0 <test-path> [...]   |   $0 --full" >&2
  exit 2
fi
if [ ! -f "$JSON" ]; then
  echo "error: $JSON not found — run '$0 --full' once to create the baseline." >&2
  exit 1
fi

TMP="$(mktemp -t wpt-report-update).json"
trap 'rm -f "$TMP"' EXIT
"$ROOT/scripts/run-wpt.sh" "$@" -- --log-wptreport="$TMP"

python3 - "$JSON" "$TMP" <<'PY'
import json, sys
full = json.load(open(sys.argv[1]))
new = json.load(open(sys.argv[2]))
idx = {r["test"]: i for i, r in enumerate(full["results"])}
updated = added = 0
for r in new["results"]:
    if r["test"] in idx:
        old = full["results"][idx[r["test"]]]["status"]
        full["results"][idx[r["test"]]] = r
        updated += 1
        if old != r["status"]:
            print(f"  {r['test']}: {old} -> {r['status']}")
    else:
        full["results"].append(r)
        added += 1
        print(f"  {r['test']}: (new) -> {r['status']}")
json.dump(full, open(sys.argv[1], "w"))
print(f"spliced {updated} updated, {added} added")
PY

python3 "$ROOT/scripts/wpt-report.py" "$JSON" "$HTML" css
