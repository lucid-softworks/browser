#!/usr/bin/env bash
#
# Run the Web Platform Tests against our engine the way other browsers are tested:
# `wpt run` driving our WebDriver server (crates/webdriver) over the standard WebDriver protocol.
#
# Usage:
#   scripts/run-wpt.sh <test-path> [<test-path> ...] [-- <extra wpt run args>]
#   scripts/run-wpt.sh fetch/api/headers
#   scripts/run-wpt.sh dom/nodes -- --log-mach-level=info
#
# Prerequisites (one-time): a WPT checkout at ./wpt with the serve infrastructure, and the WPT
# subdomains mapped to loopback. This script ensures the sparse checkout includes the needed dirs;
# the hosts mapping needs sudo and is left to you (it prints the command if resolution fails):
#   ( cd wpt && python3 ./wpt make-hosts-file | sudo tee -a /etc/hosts )
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WPT="$ROOT/wpt"
PRODUCT="lucid"

if [ ! -d "$WPT" ]; then
  echo "error: WPT checkout not found at $WPT" >&2
  echo "clone it first, e.g.: git clone --depth 1 --filter=blob:none --sparse https://github.com/web-platform-tests/wpt.git $WPT" >&2
  exit 1
fi

# Split args into test paths (before `--`) and extra `wpt run` args (after `--`).
TESTS=(); EXTRA=(); SEEN_DD=0
for a in "$@"; do
  if [ "$a" = "--" ]; then SEEN_DD=1; continue; fi
  if [ "$SEEN_DD" -eq 1 ]; then EXTRA+=("$a"); else TESTS+=("$a"); fi
done
if [ "${#TESTS[@]}" -eq 0 ]; then
  echo "usage: scripts/run-wpt.sh <test-path> [...] [-- <extra wpt run args>]" >&2
  exit 2
fi

# 1. Ensure the serve/runner infrastructure is in the sparse checkout.
( cd "$WPT" && git sparse-checkout add tools third_party docs resources common >/dev/null 2>&1 || true )

# 2. Install our product into the checkout's wptrunner.browsers package (the checkout is gitignored,
#    so the canonical copy lives in tools/wpt/). Register the product name in BUILTIN_PRODUCTS.
BROWSERS="$WPT/tools/wptrunner/wptrunner/browsers"
cp "$ROOT/tools/wpt/$PRODUCT.py" "$BROWSERS/$PRODUCT.py"
PRODUCTS_PY="$WPT/tools/wptrunner/wptrunner/products.py"
if ! grep -q "\"$PRODUCT\"" "$PRODUCTS_PY"; then
  # Register right after the "ladybird" entry in the BUILTIN_PRODUCTS frozenset.
  python3 - "$PRODUCTS_PY" "$PRODUCT" <<'PY'
import sys
path, product = sys.argv[1], sys.argv[2]
src = open(path).read()
needle = '        "ladybird",\n'
assert needle in src, "could not find insertion point in products.py"
src = src.replace(needle, needle + f'        "{product}",\n', 1)
open(path, "w").write(src)
PY
fi

# 3. The WebDriver server. Use a prebuilt binary when `$WEBDRIVER_BIN` is set and executable (CI
#    builds it once and shares it across all WPT legs); otherwise build it here.
if [ -n "${WEBDRIVER_BIN:-}" ] && [ -x "$WEBDRIVER_BIN" ]; then
  WD="$WEBDRIVER_BIN"
  echo "using prebuilt webdriver: $WD" >&2
else
  echo "building webdriver…" >&2
  cargo build --release -p webdriver --manifest-path "$ROOT/Cargo.toml" >&2
  WD="$ROOT/target/release/webdriver"
fi

# 4. Run. The webdriver binary disables the net disk-cache itself (automation must not serve stale
#    resources). `--no-pause-after-test` keeps it non-interactive.
cd "$WPT"
exec python3 ./wpt run \
  --webdriver-binary="$WD" \
  --no-pause-after-test \
  "${EXTRA[@]}" \
  "$PRODUCT" \
  "${TESTS[@]}"
