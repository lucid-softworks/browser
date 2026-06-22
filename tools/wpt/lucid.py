# mypy: allow-untyped-defs
#
# wptrunner "product" plugin for our from-scratch browser engine.
#
# It is a thin WebDriver product: wptrunner launches our `webdriver` binary
# (`crates/webdriver`, built to `target/{debug,release}/webdriver`) as
# `webdriver --port <port>`, then drives it with the standard WebDriver
# testharness/reftest/crashtest executors — exactly how Chrome/Firefox/Servo are run.
#
# This file is installed into a WPT checkout's `wptrunner.browsers` package by
# `scripts/run-wpt.sh` (the checkout is gitignored, so the canonical copy lives here).

from .base import (WebDriverBrowser,  # noqa: F401
                   get_timeout_multiplier,  # noqa: F401
                   require_arg)
from ..executors import executor_kwargs as base_executor_kwargs
from ..executors.base import PytestExecutor  # noqa: F401
from ..executors.executorwebdriver import (WebDriverTestharnessExecutor,  # noqa: F401
                                           WebDriverRefTestExecutor,  # noqa: F401
                                           WebDriverCrashtestExecutor)  # noqa: F401

__wptrunner__ = {
    "product": "lucid",
    "check_args": "check_args",
    "browser": "LucidBrowser",
    "browser_kwargs": "browser_kwargs",
    "executor_kwargs": "executor_kwargs",
    "env_options": "env_options",
    "env_extras": "env_extras",
    "timeout_multiplier": "get_timeout_multiplier",
    "executor": {
        "testharness": "WebDriverTestharnessExecutor",
        "reftest": "WebDriverRefTestExecutor",
        "wdspec": "PytestExecutor",
        "crashtest": "WebDriverCrashtestExecutor",
    },
}


def check_args(**kwargs):
    require_arg(kwargs, "webdriver_binary")


def browser_kwargs(logger, test_type, run_info_data, config, **kwargs):
    # Our engine is headless and driven entirely over WebDriver, so the browser
    # "binary" is irrelevant — default it to the webdriver binary so callers only
    # need to pass `--webdriver-binary`.
    return {"binary": kwargs.get("binary") or kwargs["webdriver_binary"],
            "webdriver_binary": kwargs["webdriver_binary"],
            "webdriver_args": kwargs.get("webdriver_args") or []}


def executor_kwargs(logger, test_type, test_environment, run_info_data, **kwargs):
    executor_kwargs = base_executor_kwargs(test_type, test_environment, run_info_data, **kwargs)
    executor_kwargs["capabilities"] = {}
    return executor_kwargs


def env_options():
    return {}


def env_extras(**kwargs):
    return []


class LucidBrowser(WebDriverBrowser):
    def make_command(self):
        return [self.webdriver_binary, "--port", str(self.port)] + self.webdriver_args
