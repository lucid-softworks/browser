#!/usr/bin/env python3
"""Ingest wpt's structured (mozlog `--log-raw`) stream into a per-test result store, live.

Usage: wpt run … --log-raw=- | scripts/wpt-ingest.py <store-dir>

Reads newline-delimited mozlog actions from stdin and, as each test finishes, writes that one test's
result to `<store-dir>/<test-path>.json` — so the store fills in as the run progresses and the report
can be regenerated mid-run to watch results land. Each file holds a single result object shaped like
a wptreport entry: {test, status, message, subtests:[{name,status,message}, …]}.

Subtest statuses arrive as `test_status` actions before the test's `test_end`; we accumulate them
per test and flush on `test_end`. Writes are atomic (tmp + os.replace) so a concurrent report
regeneration never reads a half-written file.
"""
import json
import os
import sys
from urllib.parse import quote


def store_path(store, test):
    # Mirror the test tree on disk for browsability; percent-encode anything unsafe in a path
    # segment (e.g. `?` in variant URLs) so the filename is always valid and collision-free.
    parts = [quote(p, safe="") for p in test.lstrip("/").split("/") if p]
    if not parts:
        parts = ["_root_"]
    return os.path.join(store, *parts) + ".json"


def write_result(store, result):
    p = store_path(store, result["test"])
    os.makedirs(os.path.dirname(p), exist_ok=True)
    tmp = p + ".tmp"
    with open(tmp, "w") as f:
        json.dump(result, f)
    os.replace(tmp, p)


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    store = sys.argv[1]
    os.makedirs(store, exist_ok=True)
    pending = {}  # test -> list of subtest dicts
    done = 0
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            ev = json.loads(line)
        except ValueError:
            continue
        action = ev.get("action")
        if action == "test_start":
            pending[ev.get("test")] = []
        elif action == "test_status":
            pending.setdefault(ev.get("test"), []).append({
                "name": ev.get("subtest", ""),
                "status": ev.get("status", ""),
                "message": ev.get("message"),
            })
        elif action == "test_end":
            test = ev.get("test")
            subs = pending.pop(test, [])
            write_result(store, {
                "test": test,
                "status": ev.get("status", ""),
                "message": ev.get("message"),
                "subtests": subs,
            })
            done += 1
            if done % 200 == 0:
                print(f"  …ingested {done} tests", file=sys.stderr)
    print(f"ingest: wrote {done} test results to {store}", file=sys.stderr)


if __name__ == "__main__":
    main()
