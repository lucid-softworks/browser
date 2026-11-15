#!/usr/bin/env bash
# Run the fixed PR smoke selection through the real harness; retain failures for base comparison.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:?usage: run-wpt-smoke.sh REPORT_DIRECTORY}"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
: "${WEBDRIVER_BIN:?set WEBDRIVER_BIN to the built engine under test}"
: "${ENGINE_REVISION:?set ENGINE_REVISION to its commit SHA}"
[[ -x "$WEBDRIVER_BIN" ]] || { echo 'WebDriver binary is not executable' >&2; exit 2; }
cd "$ROOT"
# Never accept a previous run's report after an early harness failure.
rm -f "$OUT/report.json" "$OUT/run.json" "$OUT/raw.log" "$OUT/harness.log"
mapfile -t tests < <(sed -e '/^[[:space:]]*#/d' -e '/^[[:space:]]*$/d' tools/wpt/smoke-tests.txt)
code=0
timeout --kill-after=15s "${WPT_SMOKE_TIMEOUT:-900}" scripts/run-wpt.sh "${tests[@]}" -- \
  --test-types=testharness --processes=1 --timeout-multiplier=1 \
  --log-wptreport="$OUT/report.json" --log-raw="$OUT/raw.log" \
  > "$OUT/harness.log" 2>&1 || code=$?
python3 - "$OUT/run.json" "$ENGINE_REVISION" "$(git -C wpt rev-parse HEAD)" "$code" "${tests[@]}" <<'PY'
import json, sys
path, engine, wpt, code, *tests = sys.argv[1:]
with open(path, 'w') as output:
    json.dump(dict(engine_revision=engine, wpt_revision=wpt, exit_code=int(code),
                   requested_tests=['/' + test.lstrip('/') for test in tests]), output, indent=2)
PY
# Exit 1 from wpt run can mean known assertion failures; the comparator owns that decision.
# Cutoffs, missing reports, missing tests and malformed reports fail here, even on both engines.
python3 scripts/compare-wpt.py "$OUT"
