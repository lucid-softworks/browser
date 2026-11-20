#!/usr/bin/env python3
"""Run live website journeys against Browser's WebDriver (Python standard library only)."""

import argparse
import base64
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
ELEMENT = "element-6066-11e4-a52e-4f735466cecf"


class DriverError(Exception):
    pass


class Driver:
    def __init__(self, port, deadline, request_timeout):
        self.base = f"http://127.0.0.1:{port}"
        self.deadline = deadline
        self.request_timeout = request_timeout
        self.session = None
        # Proxy configuration for external sites must not route localhost WebDriver traffic.
        self.http = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def request(self, method, path, data=None):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise DriverError("Site deadline exceeded")
        body = None if data is None else json.dumps(data).encode()
        req = urllib.request.Request(self.base + path, data=body, method=method,
                                     headers={"Content-Type": "application/json"})
        try:
            with self.http.open(req, timeout=min(self.request_timeout, remaining)) as response:
                payload = json.load(response)
        except urllib.error.HTTPError as error:
            raise DriverError(f"{method} {path}: HTTP {error.code}: "
                              f"{error.read().decode(errors='replace')[:2000]}") from error
        except (OSError, ValueError) as error:
            raise DriverError(f"{method} {path}: {error}") from error
        value = payload.get("value")
        if isinstance(value, dict) and value.get("error"):
            raise DriverError(f"{value['error']}: {value.get('message', '')}")
        return value

    def command(self, method, suffix, data=None):
        return self.request(method, f"/session/{self.session}/{suffix}", data)

    def element(self, selector):
        value = self.command("POST", "element", {"using": "css selector", "value": selector})
        return value[ELEMENT]

    def observe(self, step):
        # This script only reads DOM state. All interactions go through WebDriver.
        value = self.command("POST", "execute/sync", {
            "script": "var nodes = document.querySelectorAll(arguments[0]);"
                      "var texts = []; var rendered = 0;"
                      "for (var i = 0; i < nodes.length; i++) {"
                      "texts.push(nodes[i].textContent);"
                      "var rect = nodes[i].getBoundingClientRect();"
                      "var visible = rect.width > 0 && rect.height > 0;"
                      "for (var p = nodes[i]; visible && p; p = p.parentElement) {"
                      "var style = getComputedStyle(p);"
                      "if (style.display === 'none' || style.visibility === 'hidden'"
                      "|| style.visibility === 'collapse'"
                      "|| (style.opacity && Number(style.opacity) === 0)) visible = false; }"
                      "if (visible) rendered++; }"
                      "var css = {}; if (nodes.length) {"
                      "var style = getComputedStyle(nodes[0]);"
                      "for (var property in arguments[1]) css[property] = style.getPropertyValue(property); }"
                      "return {count: nodes.length, rendered_count: rendered, text: texts.join(' '), css: css};",
            "args": [step.get("selector", "html"), step.get("css", {})],
        })
        value["url"] = self.command("GET", "url")
        return value

    def assert_state(self, step, wait_seconds):
        end = min(self.deadline, time.monotonic() + wait_seconds)
        last = None
        while True:
            last = self.observe(step)
            matches = (
                ("count" not in step or last["count"] == step["count"])
                and ("min_count" not in step or last["count"] >= step["min_count"])
                and ("min_rendered_count" not in step
                     or last["rendered_count"] >= step["min_rendered_count"])
                and ("text" not in step or step["text"] in last["text"])
                and ("url_contains" not in step or step["url_contains"] in last["url"])
                and all(last["css"].get(key) == value for key, value in step.get("css", {}).items())
            )
            if matches:
                return last
            if time.monotonic() >= end:
                raise DriverError(f"Expected {json.dumps(step, ensure_ascii=False)}; "
                                  f"observed {json.dumps(last, ensure_ascii=False)[:3000]}")
            time.sleep(min(0.25, max(0, end - time.monotonic())))


def run_step(driver, step, wait_seconds):
    action = step["action"]
    if action == "navigate":
        driver.command("POST", "url", {"url": step["url"]})
    elif action == "assert":
        return driver.assert_state(step, wait_seconds)
    elif action in ("click", "type"):
        element = driver.element(step["selector"])
        rect = driver.command("GET", f"element/{element}/rect")
        suffix = "click" if action == "click" else "value"
        data = {} if action == "click" else {"text": step["text"]}
        driver.command("POST", f"element/{element}/{suffix}", data)
        return {"selector": step["selector"], "target_rect": rect}
    elif action == "back":
        driver.command("POST", "back", {})
    else:
        raise DriverError(f"Unknown action {action!r}")


def capture(driver, directory):
    results = {}
    for name, endpoint in (("page.html", "source"), ("screenshot.png", "screenshot"),
                           ("url.txt", "url")):
        try:
            value = driver.command("GET", endpoint)
            if name.endswith(".png"):
                (directory / name).write_bytes(base64.b64decode(value, validate=True))
            else:
                (directory / name).write_text(value, encoding="utf-8")
            results[name] = "captured"
        except Exception as error:
            results[name] = f"unavailable: {error}"
    return results


def run_site(site, args, output):
    directory = output / site["id"]
    directory.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    report = {"id": site["id"], "name": site["name"], "url": site["url"],
              "status": "failed", "steps": [], "artifacts": {"driver.log": "captured"}}
    process = None
    driver = None
    # Reserve a port until immediately before spawning; each site gets a fresh process/session.
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    with (directory / "driver.log").open("wb") as log:
        try:
            env = os.environ.copy()
            # Real website smoke tests must validate TLS; the WPT driver's default disables it.
            env["WPT_INSECURE_TLS"] = "0"
            env["NET_CACHE_DIR"] = "off"
            env["BROWSER_STORAGE_DIR"] = str(directory / "storage")
            process = subprocess.Popen([str(args.webdriver), "--port", str(port)],
                                       stdout=log, stderr=subprocess.STDOUT, env=env)
            driver = Driver(port, started + args.timeout, args.request_timeout)
            startup_end = min(driver.deadline, time.monotonic() + 10)
            while True:
                if process.poll() is not None:
                    raise DriverError(f"WebDriver exited with status {process.returncode}; see driver.log")
                try:
                    driver.request("GET", "/status")
                    break
                except DriverError:
                    if time.monotonic() >= startup_end:
                        raise
                    time.sleep(0.1)
            session = driver.request("POST", "/session", {"capabilities": {"alwaysMatch": {}}})
            driver.session = session["sessionId"]
            driver.command("POST", "window/rect", {"width": 1280, "height": 900})
            steps = [{"name": "Load website", "action": "navigate", "url": site["url"]}] + site["steps"]
            for index, step in enumerate(steps):
                step_started = time.monotonic()
                result = {"name": step["name"], "action": step["action"], "status": "failed"}
                report["steps"].append(result)
                try:
                    observation = run_step(driver, step, args.wait)
                    if observation is not None:
                        result["observed"] = observation
                    result["status"] = "passed"
                    print(f"[{site['id']}] PASS {step['name']}", flush=True)
                except Exception as error:
                    result["error"] = str(error)
                    for pending in steps[index + 1:]:
                        report["steps"].append({"name": pending["name"], "action": pending["action"],
                                                "status": "not_run"})
                    raise
                finally:
                    result["duration_seconds"] = round(time.monotonic() - step_started, 3)
            report["status"] = "passed"
        except Exception as error:
            report["error"] = str(error)
            print(f"[{site['id']}] FAIL {error}", file=sys.stderr, flush=True)
        finally:
            if driver and driver.session:
                # Diagnostics have their own short budget, including after a journey timeout.
                driver.deadline = time.monotonic() + 10
                driver.request_timeout = 3
                report["artifacts"].update(capture(driver, directory))
            else:
                report["artifacts"]["page.html"] = "unavailable: no session"
                report["artifacts"]["screenshot.png"] = "unavailable: no session"
            if process and process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=2)
    report["duration_seconds"] = round(time.monotonic() - started, 3)
    (directory / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    return report


def positive(value):
    result = float(value)
    if result <= 0 or not result < float("inf"):
        raise argparse.ArgumentTypeError("must be a finite positive number")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--site", action="append", help="Site ID; repeat to select several (default: all)")
    parser.add_argument("--webdriver", type=Path, default=ROOT / "target/debug/webdriver",
                        help="Path to Browser's WebDriver binary")
    parser.add_argument("--output", type=Path, default=ROOT / "target/site-results",
                        help="New output directory (must not already exist)")
    parser.add_argument("--timeout", type=positive, default=180, help="Journey deadline per site in seconds")
    parser.add_argument("--request-timeout", type=positive, default=30, help="WebDriver HTTP request timeout")
    parser.add_argument("--wait", type=positive, default=8, help="Assertion polling deadline in seconds")
    args = parser.parse_args()
    args.webdriver = args.webdriver.expanduser().resolve()
    if not args.webdriver.is_file():
        parser.error(f"WebDriver binary not found: {args.webdriver}; run cargo build -p webdriver")
    manifest = json.loads((ROOT / "tools/sites/sites.json").read_text(encoding="utf-8"))
    selected = set(args.site or [site["id"] for site in manifest["sites"]])
    unknown = selected - {site["id"] for site in manifest["sites"]}
    if unknown:
        parser.error(f"Unknown sites: {', '.join(sorted(unknown))}")
    output = args.output.expanduser().resolve()
    if output.exists():
        parser.error(f"Output directory already exists: {output}; choose a new --output directory")
    output.mkdir(parents=True)
    report = {"schema_version": 1, "started_at": datetime.now(timezone.utc).isoformat(),
              "webdriver": str(args.webdriver), "sites": []}
    for site in manifest["sites"]:
        if site["id"] in selected:
            report["sites"].append(run_site(site, args, output))
            (output / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    failed = sum(site["status"] != "passed" for site in report["sites"])
    print(f"{len(report['sites']) - failed}/{len(report['sites'])} journeys passed; report: {output / 'report.json'}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
