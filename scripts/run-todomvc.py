#!/usr/bin/env python3
"""Run the same TodoMVC journey through native WebDriver input in three browsers."""

import argparse
import concurrent.futures
import importlib.util
import json
import os
import socket
import subprocess
import sys
import time
from datetime import UTC, datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
OUT = None
spec = importlib.util.spec_from_file_location("sites", ROOT / "scripts/run-sites.py")
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
# Read-only DOM inspection, including open shadow roots. Native WebDriver performs all input.
JS = """
function all(selector, root) {
    root = root || document;
    var out = Array.from(root.querySelectorAll(selector));
    Array.from(root.querySelectorAll('*')).forEach(function (element) {
        if (element.shadowRoot) out = out.concat(all(selector, element.shadowRoot));
    });
    return out;
}
function visible(element) {
    var rect = element.getBoundingClientRect();
    if (!rect.width || !rect.height) return false;
    for (var parent = element; parent;
         parent = parent.parentElement || (parent.getRootNode && parent.getRootNode().host)) {
        var style = getComputedStyle(parent);
        if (style.display === 'none' || style.visibility === 'hidden') return false;
    }
    return true;
}
function taskText(element) {
    var text = element.querySelector('.todo-item-text');
    return (text || element).textContent.trim();
}
var input = all('.new-todo,#new-todo,input[placeholder="What needs to be done?"]').filter(visible);
var items = all('.todo-list li,#todo-list li,li.todo,li.todo-item,#todo-list .listItem').filter(visible);
"""
STEPS = [
    "Input ready",
    "Add first task",
    "Add second task",
    "Complete first task",
    "Active filter",
    "Completed filter",
    "Clear completed",
    "All filter",
]


def run(browser, site):
    slug = site["url"].split("/examples/")[1].split("/")[0]
    folder = OUT / browser / slug
    folder.mkdir(parents=True, exist_ok=True)
    report = {
        "harness_version": 4,
        **site,
        "browser": browser,
        "checks": [],
        "status": "failed",
    }
    proc = None
    d = None
    start = time.monotonic()

    def js(code):
        return d.command("POST", "execute/sync", {"script": JS + code, "args": []})

    def wait(code, expected):
        end = time.monotonic() + 10
        while True:
            got = js(code)
            if got == expected:
                return got
            if time.monotonic() > end:
                raise m.DriverError(f"Expected {expected!r}; observed {got!r}")
            time.sleep(0.2)

    def el(code):
        node = js(code)
        if not node:
            raise m.DriverError("No matching control: " + code)
        return node[m.ELEMENT]

    def click(code):
        for attempt in range(3):
            try:
                return d.command("POST", "element/" + el(code) + "/click", {})
            except (m.DriverError, OSError, ValueError, KeyError, TypeError) as e:
                if "stale element" not in str(e) or attempt == 2:
                    raise
                time.sleep(0.2)

    def add(text):
        d.command(
            "POST", "element/" + el("return input[0];") + "/value", {"text": text}
        )
        d.command(
            "POST", "element/" + el("return input[0];") + "/value", {"text": "\ue007"}
        )

    def items(texts):
        return wait(
            "return items.map(function(e){return taskText(e);}).sort();", sorted(texts)
        )

    try:
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            port = s.getsockname()[1]
        binary = {
            "engine": os.environ.get(
                "WEBDRIVER_BIN", str(ROOT / "target/debug/webdriver")
            ),
            "firefox": os.environ.get("GECKODRIVER", "geckodriver"),
            "chrome": os.environ.get("CHROMEDRIVER", "chromedriver"),
        }[browser]
        caps = {}
        if browser == "firefox":
            caps = {
                "browserName": "firefox",
                "moz:firefoxOptions": {
                    "binary": os.environ["FIREFOX"],
                    "args": ["-headless"],
                },
            }
        if browser == "chrome":
            caps = {
                "browserName": "chrome",
                "goog:chromeOptions": {
                    "binary": os.environ["CHROME"],
                    "args": [
                        "--headless=new",
                        "--no-sandbox",
                        "--disable-dev-shm-usage",
                    ],
                },
            }
        env = {
            **os.environ,
            "NET_CACHE_DIR": "off",
            "WPT_INSECURE_TLS": "0",
            "BROWSER_STORAGE_DIR": str(folder / "storage"),
        }
        with (folder / "driver.log").open("w") as log:
            proc = subprocess.Popen(
                (
                    [binary, "--port=" + str(port)]
                    if browser == "chrome"
                    else [binary, "--port", str(port)]
                ),
                stdout=log,
                stderr=subprocess.STDOUT,
                env=env,
                start_new_session=True,
            )
        d = m.Driver(port, start + 140, 40)
        for i in range(100):
            try:
                d.request("GET", "/status")
                break
            except m.DriverError:
                if proc.poll() is not None:
                    raise m.DriverError("Driver exited; see driver.log")
                time.sleep(0.1)
        session = d.request("POST", "/session", {"capabilities": {"alwaysMatch": caps}})
        d.session = session["sessionId"]
        report["capabilities"] = session["capabilities"]
        d.command("POST", "window/rect", {"width": 1280, "height": 900})
        d.command(
            "POST", "timeouts", {"pageLoad": 30000, "script": 10000, "implicit": 0}
        )
        d.command("POST", "url", {"url": site["url"]})
        for index, name in enumerate(STEPS):
            check = {"name": name, "status": "failed"}
            report["checks"].append(check)
            try:
                if index == 0:
                    wait("return input.length;", 1)
                if index == 1:
                    add("Task Alpha")
                    items(["Task Alpha"])
                if index == 2:
                    add("Task Beta")
                    items(["Task Alpha", "Task Beta"])
                if index == 3:
                    click(
                        "return items.filter(function(e){return taskText(e)==='Task Alpha';})[0].querySelector('.toggle,input[type=checkbox]');"
                    )
                    wait(
                        "return items.filter(function(e){var c=e.querySelector('input[type=checkbox]');return c&&c.checked;}).length;",
                        1,
                    )
                if index in (4, 5, 7):
                    label = {4: "Active", 5: "Completed", 7: "All"}[index]
                    click(
                        "return all('a').filter(function(e){return visible(e)&&e.textContent.trim()==="
                        + json.dumps(label)
                        + ";})[0];"
                    )
                    items(["Task Beta"] if index in (4, 7) else ["Task Alpha"])
                if index == 6:
                    click(
                        "return all('button,.clear-completed,#clear-completed').filter(function(e){return visible(e)&&e.textContent.indexOf('Clear completed')>=0;})[0];"
                    )
                    wait(
                        "return items.filter(function(e){return e.textContent.indexOf('Task Alpha')>=0;}).length;",
                        0,
                    )
                check["status"] = "passed"
            except (m.DriverError, OSError, ValueError, KeyError, TypeError) as e:
                check["error"] = str(e)
                raise
        report["status"] = "passed"
    except (m.DriverError, OSError, ValueError, KeyError, TypeError) as e:
        report["error"] = str(e)
    finally:
        if d and d.session:
            d.deadline = time.monotonic() + 10
            d.request_timeout = 3
            report["artifacts"] = m.capture(d, folder)
            try:
                (folder / "shadow.json").write_text(
                    json.dumps(
                        js(
                            "return all('*').filter(function(e){return e.shadowRoot;}).map(function(e){return {tag:e.tagName,html:e.shadowRoot.innerHTML};});"
                        )
                    )
                )
            except (m.DriverError, OSError, ValueError) as error:
                report["artifacts"]["shadow.json"] = str(error)
            try:
                d.request("DELETE", "/session/" + d.session)
            except m.DriverError as error:
                report["session_cleanup_error"] = str(error)
        if proc:
            import signal

            try:
                os.killpg(proc.pid, signal.SIGTERM)
                proc.wait(timeout=3)
            except (OSError, subprocess.TimeoutExpired) as error:
                report["process_cleanup_note"] = str(error)
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                    proc.wait(timeout=3)
                except (OSError, subprocess.TimeoutExpired) as error:
                    report["process_cleanup_error"] = str(error)
        report["checks"] += [
            {"name": n, "status": "not_run"} for n in STEPS[len(report["checks"]) :]
        ]
        report["seconds"] = round(time.monotonic() - start, 2)
        (folder / "report.json").write_text(json.dumps(report, indent=2))
        print(
            browser,
            slug,
            report["status"],
            sum(c["status"] == "passed" for c in report["checks"]),
            report.get("error", "")[:160],
            flush=True,
        )
    return report


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("browser", choices=["engine", "chrome", "firefox"])
    parser.add_argument("--filter", default="", help="URL substring to select examples")
    parser.add_argument(
        "--output",
        type=Path,
        help="New output directory (default: timestamped target/todomvc/)",
    )
    parser.add_argument("--jobs", type=int, default=3)
    args = parser.parse_args()
    if args.jobs < 1:
        parser.error("--jobs must be positive")
    for variable in {"chrome": ["CHROME"], "firefox": ["FIREFOX"]}.get(
        args.browser, []
    ):
        if not os.environ.get(variable):
            parser.error(f"Set {variable} to the browser executable path")
    OUT = args.output or ROOT / "target/todomvc" / datetime.now(UTC).strftime(
        "%Y%m%dT%H%M%S.%fZ"
    )
    OUT = OUT.resolve()
    if (OUT / args.browser).exists():
        parser.error(
            f"{OUT / args.browser} already exists; use a new directory for fresh storage"
        )
    OUT.mkdir(parents=True, exist_ok=True)
    manifest = ROOT / "tools/sites/todomvc.json"
    examples = [s for s in json.loads(manifest.read_text()) if args.filter in s["url"]]
    if not examples:
        parser.error("No examples matched --filter")
    (OUT / "examples.json").write_text(json.dumps(examples, indent=2))
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        results = list(pool.map(lambda site: run(args.browser, site), examples))
    (OUT / (args.browser + "-results.json")).write_text(json.dumps(results, indent=2))
    print(
        f"{sum(r['status'] == 'passed' for r in results)}/{len(results)} passed. Results: {OUT}"
    )
    sys.exit(0 if all(r["status"] == "passed" for r in results) else 1)
