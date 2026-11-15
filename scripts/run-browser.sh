#!/usr/bin/env bash
# Launch the shared Rune desktop app on macOS or Linux (or run its headless tooling).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then
    echo "error: install Rust 1.99.1 or newer, then run this script again" >&2
    exit 1
fi

HEADLESS=0
NATIVE=0
for arg in "$@"; do
    case "$arg" in
        --snap|--tree|--frames|--strip|--video|--out|--list-scenes|--help|-h) HEADLESS=1 ;;
        --native) NATIVE=1 ;;
    esac
done
if [[ "$(uname -s)" == Linux && -z "${DISPLAY:-}" && -z "${WAYLAND_DISPLAY:-}" ]] \
    && [[ "$HEADLESS" == 0 || "$NATIVE" == 1 ]]; then
    echo "error: no X11 or Wayland display is available; launch from a desktop session" >&2
    echo "For a headless render, use: $0 --snap /tmp/browser.png" >&2
    exit 1
fi

cd "$ROOT"
exec cargo run --locked -p browser-desktop -- "$@"
