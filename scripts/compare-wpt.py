#!/usr/bin/env python3
"""Compare real wptreport results; existing failures are allowed, lost coverage is not."""
import argparse
import json
from pathlib import Path
import sys
from urllib.parse import urlsplit

# Harness failures are worse than assertions; a new timeout/crash must not be hidden behind
# a pre-existing FAIL. Missing outcomes are checked separately rather than given a rank.
RANK = {"OK": 0, "PASS": 0, "FAIL": 1, "PRECONDITION_FAILED": 2,
        "ERROR": 2, "NOTRUN": 3, "TIMEOUT": 4, "EXTERNAL-TIMEOUT": 4,
        "CRASH": 5, "INTERNAL-ERROR": 6}


def selected_tests(path):
    tests = [line.strip() for line in Path(path).read_text().splitlines()
             if line.strip() and not line.lstrip().startswith("#")]
    if not tests or len(tests) != len(set(tests)):
        raise ValueError("test selection must be nonempty and contain no duplicates")
    return {"/" + test.lstrip("/") for test in tests}


def load_run(directory, selected):
    directory = Path(directory)
    meta = json.loads((directory / "run.json").read_text())
    if meta["exit_code"] not in (0, 1):
        raise ValueError(f"{directory}: harness failed or was cut off (exit {meta['exit_code']})")
    if set(meta["requested_tests"]) != selected:
        raise ValueError(f"{directory}: requested test selection differs")
    report = json.loads((directory / "report.json").read_text())
    if not report.get("results") or not report.get("time_end"):
        raise ValueError(f"{directory}: empty or incomplete wptreport")
    tests = {}
    for result in report["results"]:
        test = result["test"]
        if test in tests:
            raise ValueError(f"{directory}: duplicate result {test}")
        if result["status"] not in RANK:
            raise ValueError(f"{directory}: unsupported status {result['status']}")
        subtests = {}
        for subtest in result.get("subtests", []):
            name, status = subtest["name"], subtest["status"]
            if name in subtests or status not in RANK:
                raise ValueError(f"{directory}: duplicate/invalid subtest in {test}: {name}")
            subtests[name] = status
        tests[test] = (result["status"], subtests)
    observed = {urlsplit(test).path for test in tests}
    if observed != selected:
        raise ValueError(f"{directory}: coverage differs; missing={sorted(selected-observed)}, "
                         f"extra={sorted(observed-selected)}")
    return meta, tests


def compare(base, head):
    regressions = []
    for test in sorted(base.keys() | head.keys()):
        if test not in head:
            regressions.append(f"{test}: missing test result")
            continue
        if test not in base:
            regressions.append(f"{test}: absent from baseline; comparison is incomplete")
            continue
        before, before_subtests = base[test]
        after, after_subtests = head[test]
        if RANK[after] > RANK[before]:
            regressions.append(f"{test}: {before} → {after}")
        for name, status in before_subtests.items():
            if name not in after_subtests:
                regressions.append(f"{test} :: {name}: missing subtest (was {status})")
            elif RANK[after_subtests[name]] > RANK[status]:
                regressions.append(f"{test} :: {name}: {status} → {after_subtests[name]}")
        # New passing subtests are progress, but newly observed failures still need review.
        for name in after_subtests.keys() - before_subtests.keys():
            if RANK[before] == 0 and RANK[after_subtests[name]] > 0:
                regressions.append(f"{test} :: {name}: new {after_subtests[name]} outcome")
    return regressions


def counts(tests):
    subtests = [status for _, subs in tests.values() for status in subs.values()]
    return len(tests), sum(status == "PASS" for status in subtests), len(subtests)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base")
    parser.add_argument("head", nargs="?")
    parser.add_argument("--tests", default="tools/wpt/smoke-tests.txt")
    parser.add_argument("--output", help="write a Markdown report in addition to stdout")
    args = parser.parse_args()
    try:
        selected = selected_tests(args.tests)
        base_meta, base = load_run(args.base, selected)
        if not args.head:
            print(f"Validated {counts(base)[0]} WPT results with complete selected-file coverage.")
            return 0
        head_meta, head = load_run(args.head, selected)
        if base_meta["wpt_revision"] != head_meta["wpt_revision"]:
            raise ValueError("base and head used different WPT revisions")
        if not counts(base)[1]:
            raise ValueError("baseline has no passing subtests; cannot establish a useful baseline")
        regressions = compare(base, head)
        lines = ["## WPT smoke comparison", "",
                 f"WPT revision: `{base_meta['wpt_revision']}`", "",
                 "| Engine | Revision | Tests | Passing subtests | Total subtests |",
                 "| --- | --- | ---: | ---: | ---: |"]
        for label, meta, tests in [("Base", base_meta, base), ("Head", head_meta, head)]:
            n, passed, total = counts(tests)
            lines.append(f"| {label} | `{meta['engine_revision']}` | {n} | {passed} | {total} |")
        lines += ["", f"**{len(regressions)} regression(s).**", ""]
        # Avoid treating arbitrary test names as Markdown/HTML in the runner summary.
        lines += ["```text", *[line.replace("```", "''' ") for line in regressions[:200]], "```"] if regressions else []
        if len(regressions) > 200:
            lines.append("Only the first 200 are shown; consult the raw reports for all outcomes.")
        output = "\n".join(lines) + "\n"
        print(output)
        if args.output:
            Path(args.output).write_text(output)
        return int(bool(regressions))
    except (OSError, KeyError, TypeError, ValueError) as error:
        message = f"WPT comparison failed: {error}\n"
        print(message, file=sys.stderr)
        if args.output:
            Path(args.output).write_text(message)
        return 2


if __name__ == "__main__":
    sys.exit(main())
