#!/usr/bin/env bash
# Build the Rust engine (static lib + C header) then the Swift app that links it.
#   ./scripts/build.sh                 debug build (default)
#   ./scripts/build.sh release         optimized build, bare executable
#   ./scripts/build.sh release-app     optimized build, packaged + signed Browser.app
#                                       set NOTARIZE=1 to also notarize + staple
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODE="${1:-debug}"   # debug | release | release-app
case "$MODE" in
    release|release-app) RELEASE=1 ;;
    debug)               RELEASE=0 ;;
    *) echo "Unknown mode: $MODE (use: debug | release | release-app)"; exit 1 ;;
esac

echo "==> Building Rust engine ($MODE)"
if [[ "$RELEASE" == 1 ]]; then
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
# Package.swift hardcodes `-L <ROOT>/target/debug -lbrowser_ffi`. Two consequences that silently
# ship a STALE engine:
#   1. The linker prefers the CDYLIB over the .a, so the app dynamically loads
#      target/debug/deps/libbrowser_ffi.dylib at runtime — and a release `cargo build` updates
#      target/release/..., NOT target/debug/..., so the app keeps loading the old dylib.
#   2. The linker searches target/debug FIRST, so even the static path resolves there.
# Fix: mirror the freshly-built lib (whatever MODE) into every target/debug location the app's
# Package.swift -L and the binary's baked dylib load path use.
for d in "$ROOT/target/debug" "$ROOT/target/debug/deps"; do
    mkdir -p "$d"
    for ext in a dylib; do
        src="$RUST_LIBDIR/libbrowser_ffi.$ext"; [[ -f "$src" ]] || src="$RUST_LIBDIR/deps/libbrowser_ffi.$ext"
        [[ -f "$src" ]] && cp -f "$src" "$d/libbrowser_ffi.$ext"
    done
done
# SwiftPM doesn't track the .a/.dylib as a dependency and caches the resolved lib, so an
# incremental build keeps linking a stale lib. A clean Swift build re-reads it. ~5s; correctness.
rm -rf "$ROOT/swift/.build"
if [[ "$RELEASE" == 1 ]]; then
    swift build --package-path "$ROOT/swift" -c release \
        -Xlinker -L"$RUST_LIBDIR"
    APP="$ROOT/swift/.build/release/Browser"
else
    swift build --package-path "$ROOT/swift"
    APP="$ROOT/swift/.build/debug/Browser"
fi

echo "==> Built: $APP"

# --- Package + sign a distributable Browser.app -------------------------------------------------
if [[ "$MODE" == "release-app" ]]; then
    SIGN_ID="${SIGN_ID:-Developer ID Application: Lucid Softworks limited (3TH63SHTY2)}"
    PKG="$ROOT/packaging"
    DIST="$ROOT/dist"
    BUNDLE="$DIST/Browser.app"
    MACOS="$BUNDLE/Contents/MacOS"
    FRAMEWORKS="$BUNDLE/Contents/Frameworks"

    echo "==> Packaging $BUNDLE"
    rm -rf "$BUNDLE"
    mkdir -p "$MACOS" "$FRAMEWORKS" "$BUNDLE/Contents/Resources"
    cp "$APP" "$MACOS/Browser"
    cp "$PKG/Info.plist" "$BUNDLE/Contents/Info.plist"

    # The engine dylib is self-contained (V8 statically inside, only system deps). Bundle it and
    # rewrite its absolute build-path load command to an @rpath relative to the executable.
    DYLIB="$RUST_LIBDIR/libbrowser_ffi.dylib"; [[ -f "$DYLIB" ]] || DYLIB="$RUST_LIBDIR/deps/libbrowser_ffi.dylib"
    cp "$DYLIB" "$FRAMEWORKS/libbrowser_ffi.dylib"
    install_name_tool -id "@rpath/libbrowser_ffi.dylib" "$FRAMEWORKS/libbrowser_ffi.dylib"
    OLD_REF="$(otool -L "$MACOS/Browser" | awk '/libbrowser_ffi\.dylib/{print $1; exit}')"
    [[ -n "$OLD_REF" ]] && install_name_tool -change "$OLD_REF" "@rpath/libbrowser_ffi.dylib" "$MACOS/Browser"
    install_name_tool -add_rpath "@executable_path/../Frameworks" "$MACOS/Browser" 2>/dev/null || true

    # Sign inside-out: the nested dylib first, then the app bundle (whose signature seals it). The
    # main executable carries the JIT entitlements; the hardened runtime is required for notarization.
    # SIGN_ID is overridable so CI can import the Developer ID cert into a temporary keychain.
    echo "==> Signing with: $SIGN_ID"
    codesign --force --options runtime --timestamp --sign "$SIGN_ID" "$FRAMEWORKS/libbrowser_ffi.dylib"
    codesign --force --options runtime --timestamp \
        --entitlements "$PKG/Browser.entitlements" \
        --sign "$SIGN_ID" "$BUNDLE"
    codesign --verify --strict --verbose=2 "$BUNDLE"
    echo "==> Signed: $BUNDLE"

    # Notarize only on request (needs Apple credentials). Two auth paths:
    #   * Local: a stored notarytool profile (NOTARY_PROFILE, default "lucid-notary"), created with
    #       xcrun notarytool store-credentials lucid-notary \
    #           --apple-id luna@lucidsoft.works --team-id 3TH63SHTY2 --password <app-specific-password>
    #   * CI: env vars NOTARY_APPLE_ID + NOTARY_TEAM_ID + NOTARY_PASSWORD (GitHub secrets).
    if [[ "${NOTARIZE:-0}" == "1" ]]; then
        ZIP="$DIST/Browser.zip"
        echo "==> Notarizing"
        ditto -c -k --keepParent "$BUNDLE" "$ZIP"
        if [[ -n "${NOTARY_APPLE_ID:-}" && -n "${NOTARY_TEAM_ID:-}" && -n "${NOTARY_PASSWORD:-}" ]]; then
            xcrun notarytool submit "$ZIP" --apple-id "$NOTARY_APPLE_ID" \
                --team-id "$NOTARY_TEAM_ID" --password "$NOTARY_PASSWORD" --wait
        else
            xcrun notarytool submit "$ZIP" --keychain-profile "${NOTARY_PROFILE:-lucid-notary}" --wait
        fi
        xcrun stapler staple "$BUNDLE"
        spctl --assess --type execute --verbose=4 "$BUNDLE" || true
        echo "==> Notarized + stapled: $BUNDLE"
    else
        echo "==> Skipped notarization. Set NOTARIZE=1 (after configuring credentials)."
    fi
    echo "==> App: $BUNDLE"
else
    echo "Run it with: $APP"
fi
