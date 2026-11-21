#!/usr/bin/env python3
"""Drive an unmodified Speedometer 3.1 checkout, sequentially, via WebDriver."""

import argparse
import functools
import hashlib
import http.server
import importlib.util
import json
import math
import os
import platform
import signal
import socket
import subprocess
import threading
import time
import urllib.parse
from datetime import UTC, datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("sites", ROOT / "scripts/run-sites.py")
sites = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sites)


def run(args, url, browser, repeat):
    folder = args.output / f"{browser}-{repeat}"
    folder.mkdir(parents=True, exist_ok=False)
    report = {"browser": browser, "repeat": repeat, "url": url, "status": "failed"}
    report["started_at"] = datetime.now(UTC).isoformat()
    report["measurement_scope"] = (
        "diagnostic: iframe JavaScript, layout, rasterization and compositing"
        if browser == "engine"
        else "headless browser"
    )
    report["engine_task_budget_seconds"] = args.engine_task_budget
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    binaries = {
        "engine": os.environ.get(
            "WEBDRIVER_BIN", str(ROOT / "target/release/webdriver")
        ),
        "chrome": os.environ.get("CHROMEDRIVER", "chromedriver"),
        "firefox": os.environ.get("GECKODRIVER", "geckodriver"),
    }
    caps = {}
    if browser == "chrome":
        caps = {
            "browserName": "chrome",
            "goog:chromeOptions": {
                "binary": os.environ["CHROME"],
                "args": ["--headless=new", "--no-sandbox", "--disable-dev-shm-usage"],
            },
        }
    if browser == "firefox":
        caps = {
            "browserName": "firefox",
            "moz:firefoxOptions": {
                "binary": os.environ["FIREFOX"],
                "args": ["-headless"],
            },
        }
    proc = None
    if browser == "engine":
        with open(binaries[browser], "rb") as binary:
            report["binary_sha256"] = hashlib.file_digest(binary, "sha256").hexdigest()
    driver = sites.Driver(port, time.monotonic() + args.timeout, args.timeout)
    try:
        with (folder / "driver.log").open("w") as log:
            command = (
                [binaries[browser], f"--port={port}"]
                if browser == "chrome"
                else [binaries[browser], "--port", str(port)]
            )
            proc = subprocess.Popen(
                command,
                stdout=log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
                env={
                    **os.environ,
                    "BROWSER_STORAGE_DIR": str(folder / "storage"),
                    "BROWSER_SCRIPT_BUDGET_MS": str(args.engine_task_budget * 1000),
                },
            )
        for _ in range(100):
            try:
                driver.request("GET", "/status")
                break
            except sites.DriverError:
                if proc.poll() is not None:
                    raise RuntimeError("Driver exited")
                time.sleep(0.1)
        session = driver.request(
            "POST", "/session", {"capabilities": {"alwaysMatch": caps}}
        )
        driver.session = session["sessionId"]
        report["capabilities"] = session["capabilities"]
        driver.command("POST", "window/rect", {"width": 1280, "height": 900})
        # A poll can share an engine event-loop drain with a long benchmark task. Give it
        # the configured task budget plus handoff time; the whole-run deadline still applies.
        poll_timeout = args.engine_task_budget + 5 if browser == "engine" else 30
        driver.command(
            "POST",
            "timeouts",
            {"script": min(args.timeout, poll_timeout) * 1000, "pageLoad": 120000},
        )
        driver.command("POST", "url", {"url": url})

        def js(script):
            return driver.command(
                "POST", "execute/sync", {"script": script, "args": []}
            )

        report["initial"] = js(
            "return {client:typeof benchmarkClient, text:document.body.innerText, errors:globalThis.__timerErrors||[]};"
        )
        if report["initial"]["client"] == "undefined":
            raise RuntimeError("Speedometer client did not initialize")
        configuration = js(
            """
            globalThis.__benchmarkError = null;
            var oldError = benchmarkClient.handleError;
            benchmarkClient.handleError = function(e) {
                globalThis.__benchmarkError = String(e.stack || e);
                return oldError.call(this, e);
            };
            var oldStep = benchmarkClient.didRunTest;
            benchmarkClient.didRunTest = function(suite, test) {
                // Engine event exceptions are recorded in the realm where they occurred.
                // Inspect them before the runner discards the workload's iframe.
                var frames = document.querySelectorAll('iframe.test-runner');
                for (var i = 0; i < frames.length; i++) {
                    var errors = frames[i].contentWindow.__timerErrors || [];
                    if (errors.length)
                        globalThis.__benchmarkError = suite.name + '/' + test.name + ': ' + errors.join('\\n');
                }
                return oldStep.call(this, suite, test);
            };
            benchmarkClient.start();
            return {steps:benchmarkClient.stepCount, suites:benchmarkClient.suitesCount};
            """
        )
        report["configuration"] = configuration
        if args.suite and configuration["suites"] != 1:
            raise RuntimeError("Requested workload selection was not applied")
        while True:
            state = driver.command(
                "POST",
                "execute/async",
                {
                    "script": "var done=arguments[arguments.length-1]; setTimeout(function(){done({done:benchmarkClient._hasResults, running:benchmarkClient._isRunning, finished:benchmarkClient._finishedTestCount, error:globalThis.__benchmarkError, suite:document.getElementById('info-label').textContent, errors:globalThis.__timerErrors||[]});},1000);",
                    "args": [],
                },
            )
            report["state"] = state
            (folder / "progress.json").write_text(json.dumps(state, indent=2))
            if not isinstance(state, dict):
                raise TypeError(
                    "WebDriver returned no benchmark state; see diagnostics"
                )
            if state.get("errors"):
                raise RuntimeError("Benchmark task failed: " + str(state["errors"]))
            if state.get("error"):
                raise RuntimeError(state["error"])
            if state["done"]:
                report["metrics"] = js("return benchmarkClient.metrics;")
                report["iterations"] = js("return benchmarkClient._measuredValuesList;")
                if state["finished"] != configuration["steps"]:
                    raise RuntimeError(
                        "Benchmark did not complete every requested step"
                    )
                if len(report["iterations"]) != args.iterations:
                    raise RuntimeError(
                        "Benchmark did not run the requested iteration count"
                    )
                score = report["metrics"].get("Score", {}).get("mean")
                if (
                    not isinstance(score, (int, float))
                    or not math.isfinite(score)
                    or score <= 0
                ):
                    raise RuntimeError("Benchmark returned no valid positive score")
                report["status"] = "passed"
                break
            if not state["running"]:
                raise RuntimeError("Benchmark stopped before producing results")
    except (
        sites.DriverError,
        RuntimeError,
        OSError,
        ValueError,
        TypeError,
        KeyError,
    ) as error:
        report["error"] = str(error)
    finally:
        if driver.session:
            driver.deadline = time.monotonic() + 5
            driver.request_timeout = 5
            try:
                report["diagnostics"] = driver.command(
                    "POST",
                    "execute/sync",
                    {
                        "script": "return {text:document.body.innerText, errors:globalThis.__timerErrors||[], frames:Array.from(document.querySelectorAll('iframe.test-runner'), function(f){return {url:f.src, errors:f.contentWindow.__timerErrors||[], text:f.contentDocument.body.innerText.slice(0,2000)};})};",
                        "args": [],
                    },
                )
                driver.request("DELETE", f"/session/{driver.session}")
            except sites.DriverError as error:
                report["cleanup_error"] = str(error)
        if proc:
            try:
                os.killpg(proc.pid, signal.SIGTERM)
                proc.wait(timeout=5)
            except (ProcessLookupError, subprocess.TimeoutExpired):
                if proc.poll() is None:
                    os.killpg(proc.pid, signal.SIGKILL)
                    proc.wait()
        (folder / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(
        browser,
        repeat,
        report["status"],
        report.get("error", report.get("state")),
        flush=True,
    )
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--source", type=Path, default=ROOT / "target/speedometer-source"
    )
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--browsers",
        nargs="+",
        choices=["engine", "chrome", "firefox"],
        default=["engine", "chrome", "firefox"],
    )
    parser.add_argument("--iterations", type=int, default=10)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--suite")
    parser.add_argument("--timeout", type=int, default=1800)
    parser.add_argument(
        "--engine-task-budget",
        type=int,
        default=180,
        choices=range(1, 601),
        metavar="SECONDS",
    )
    args = parser.parse_args()
    if min(args.iterations, args.repeats, args.timeout) < 1:
        parser.error("iterations, repeats and timeout must be positive")
    args.source = args.source.resolve()
    if not (args.source / "resources/main.mjs").is_file():
        parser.error("source must be a built Speedometer 3.1 checkout")
    if subprocess.check_output(
        ["git", "-C", str(args.source), "status", "--porcelain"], text=True
    ).strip():
        parser.error("benchmark source must be unmodified")
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    metadata = {
        "started_at": datetime.now(UTC).isoformat(),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "iterations": args.iterations,
        "repeats": args.repeats,
        "suite": args.suite,
        "headless": True,
        "outer_window": [1280, 900],
        "source_revision": subprocess.check_output(
            ["git", "-C", str(args.source), "rev-parse", "HEAD"], text=True
        ).strip(),
        "engine_revision": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
        ).strip(),
        "engine_diff": subprocess.check_output(["git", "diff"], cwd=ROOT, text=True),
    }
    (args.output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    handler = functools.partial(
        http.server.SimpleHTTPRequestHandler, directory=str(args.source.resolve())
    )
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    params = {"iterationCount": args.iterations}
    if args.suite:
        params["suite"] = args.suite
    url = f"http://127.0.0.1:{server.server_port}/?" + urllib.parse.urlencode(params)
    reports = []
    try:
        for repeat in range(1, args.repeats + 1):
            for browser in args.browsers:
                reports.append(run(args, url, browser, repeat))
                (args.output / "results.json").write_text(
                    json.dumps(reports, indent=2) + "\n"
                )
    finally:
        server.shutdown()
        server.server_close()
    return int(any(r["status"] != "passed" for r in reports))


if __name__ == "__main__":
    raise SystemExit(main())
