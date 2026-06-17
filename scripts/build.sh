#!/usr/bin/env bash
# Build the Rust engine (static lib + C header) then the Swift app that links it.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODE="${1:-debug}"   # debug | release

echo "==> Building Rust engine ($MODE)"
if [[ "$MODE" == "release" ]]; then
    cargo build --release --manifest-path "$ROOT/Cargo.toml"
    RUST_LIBDIR="$ROOT/target/release"
else
    cargo build --manifest-path "$ROOT/Cargo.toml"
    RUST_LIBDIR="$ROOT/target/debug"
fi

echo "==> Header at $ROOT/include/browser.h"
test -f "$ROOT/include/browser.h"
test -f "$RUST_LIBDIR/libbrowser_ffi.a"

echo "==> Building Swift app"
# The Swift package links against $RUST_LIBDIR via -L in Package.swift (debug path).
# For release, override the search path on the command line.
if [[ "$MODE" == "release" ]]; then
    swift build --package-path "$ROOT/swift" -c release \
        -Xlinker -L"$RUST_LIBDIR"
    APP="$ROOT/swift/.build/release/Browser"
else
    swift build --package-path "$ROOT/swift"
    APP="$ROOT/swift/.build/debug/Browser"
fi

echo "==> Built: $APP"
echo "Run it with: $APP"
