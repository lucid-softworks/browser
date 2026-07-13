#!/usr/bin/env python3
"""Generate a bounded GitHub Actions matrix covering the complete WPT tree.

Large top-level areas are split at their first directory boundary and then hash-chunked. Small
areas are packed together so the matrix stays below GitHub's 256-job limit. Each entry carries
separate test and sparse-checkout paths: root-level tests can therefore include their area's
resource directories without accidentally selecting those resources as tests.
"""

import argparse
import json
import math
import os
import re
import subprocess
import sys
from collections import defaultdict


INFRA_TOP_LEVEL = {
    ".github",
    ".well-known",
    "common",
    "docs",
    "images",
    "interfaces",
    "resources",
    "third_party",
    "tools",
}
DOCUMENT_SUFFIXES = (".htm", ".html", ".xht", ".xhtml", ".svg", ".xml")
JS_TEST_PATTERN = re.compile(
    r"\.(?:any|window|worker|sharedworker|serviceworker|audioworklet|paintworklet)(?:\.[^.]+)*\.js$"
)


def git_paths(repo):
    output = subprocess.check_output(
        ["git", "-C", repo, "ls-tree", "-r", "--name-only", "HEAD"], text=True
    )
    return [path for path in output.splitlines() if path]


def is_test_candidate(path):
    """Approximate WPT manifest entries closely enough to balance shards.

    False positives only make a shard smaller; wptrunner remains the authority on what is a test.
    """
    name = os.path.basename(path)
    if name.startswith(".") or name.endswith("-ref.html") or name.endswith("-notref.html"):
        return False
    if name.endswith(DOCUMENT_SUFFIXES):
        return True
    if path.startswith("webdriver/tests/") and name.endswith(".py"):
        return name not in {"__init__.py", "conftest.py"}
    return bool(JS_TEST_PATTERN.search(name))


def resource_paths(all_paths, top):
    prefixes = (f"{top}/resources/", f"{top}/support/")
    found = []
    for prefix in prefixes:
        if any(path.startswith(prefix) for path in all_paths):
            found.append(prefix.rstrip("/"))
    return found


def build_units(all_paths, target):
    candidates = [
        path
        for path in all_paths
        if "/" in path
        and path.split("/", 1)[0] not in INFRA_TOP_LEVEL
        and is_test_candidate(path)
    ]
    by_top = defaultdict(list)
    for path in candidates:
        by_top[path.split("/", 1)[0]].append(path)

    units = []
    for top, paths in sorted(by_top.items()):
        resources = resource_paths(all_paths, top)
        if len(paths) <= target:
            units.append({"tests": [top], "checkout": [top], "weight": len(paths)})
            continue

        root_files = []
        by_child = defaultdict(list)
        for path in paths:
            rest = path[len(top) + 1 :]
            if "/" in rest:
                by_child[rest.split("/", 1)[0]].append(path)
            else:
                root_files.append(path)

        if root_files:
            units.append(
                {
                    "tests": root_files,
                    "checkout": root_files + resources,
                    "weight": len(root_files),
                }
            )
        for child, child_paths in sorted(by_child.items()):
            path = f"{top}/{child}"
            units.append(
                {
                    "tests": [path],
                    "checkout": [path] + resources,
                    "weight": len(child_paths),
                }
            )
    return units, len(candidates)


def pack_matrix(units, target):
    matrix = []
    small = []
    for unit in units:
        chunks = math.ceil(unit["weight"] / target)
        if chunks > 1:
            for chunk in range(1, chunks + 1):
                matrix.append(
                    {
                        "tests": unit["tests"],
                        "checkout": list(dict.fromkeys(unit["checkout"])),
                        "chunks": chunks,
                        "chunk": chunk,
                    }
                )
        else:
            small.append(unit)

    bins = []
    for unit in sorted(small, key=lambda item: item["weight"], reverse=True):
        for bucket in bins:
            if bucket["weight"] + unit["weight"] <= target:
                bucket["units"].append(unit)
                bucket["weight"] += unit["weight"]
                break
        else:
            bins.append({"units": [unit], "weight": unit["weight"]})

    for bucket in bins:
        tests = []
        checkout = []
        for unit in bucket["units"]:
            tests.extend(unit["tests"])
            checkout.extend(unit["checkout"])
        matrix.append(
            {
                "tests": tests,
                "checkout": list(dict.fromkeys(checkout)),
                "chunks": 1,
                "chunk": 1,
            }
        )

    for index, entry in enumerate(matrix, 1):
        entry["slug"] = f"shard-{index:03d}"
    return matrix


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("repo", help="WPT git checkout")
    parser.add_argument("--target", type=int, default=400, help="candidate tests per shard")
    parser.add_argument("--max-shards", type=int, default=240)
    args = parser.parse_args()

    paths = git_paths(args.repo)
    target = args.target
    while True:
        units, candidate_count = build_units(paths, target)
        matrix = pack_matrix(units, target)
        if len(matrix) <= args.max_shards:
            break
        target = math.ceil(target * 1.15)

    if not matrix:
        raise SystemExit("no WPT test candidates found")
    print(json.dumps({"include": matrix}, separators=(",", ":")))
    print(
        f"WPT matrix: {candidate_count} candidate tests, {len(units)} path units, "
        f"{len(matrix)} shards, target {target}",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
