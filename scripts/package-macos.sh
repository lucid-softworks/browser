#!/usr/bin/env bash
# Package the Rune desktop executable as a drag-to-Applications disk image.
# Usage: package-macos.sh BINARY OUTPUT_DIR VERSION ARCH BUILD_SHA
# ARCH is arm64 or x86_64. Build with MACOSX_DEPLOYMENT_TARGET=13.0.
# Uses an ad-hoc signature for testing; this is not Developer ID signing/notarization.
set -euo pipefail

if [[ $# != 5 ]]; then
    echo "Usage: $0 BINARY OUTPUT_DIR VERSION ARCH BUILD_SHA" >&2
    exit 2
fi
if [[ "$(uname -s)" != Darwin ]]; then
    echo "macOS packaging requires macOS (codesign, lipo, and hdiutil)." >&2
    exit 2
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BINARY="$1"
OUTPUT_DIR="$2"
VERSION="$3"
ARCH="$4"
BUILD_SHA="$5"
[[ -f "$BINARY" ]] || { echo "Missing executable: $BINARY" >&2; exit 2; }
[[ "$VERSION" =~ ^[0-9]+(\.[0-9]+){0,2}$ ]] || {
    echo "VERSION must contain one to three numeric components." >&2; exit 2;
}
[[ "$ARCH" == arm64 || "$ARCH" == x86_64 ]] || {
    echo "ARCH must be arm64 or x86_64." >&2; exit 2;
}
[[ "$BUILD_SHA" =~ ^[0-9a-fA-F]{7,40}$ ]] || {
    echo "BUILD_SHA must be a Git commit hash." >&2; exit 2;
}
lipo "$BINARY" -verify_arch "$ARCH"

# A CI runner's Homebrew/build-directory dylib must never leak into the downloadable app.
# The Rune executable is statically linked except for macOS system libraries/frameworks.
dependencies="$(otool -L "$BINARY")"
while IFS= read -r dependency; do
    case "$dependency" in
        /System/Library/*|/usr/lib/*) ;;
        *) echo "Non-system dependency cannot be shipped unbundled: $dependency" >&2; exit 1 ;;
    esac
done < <(printf '%s\n' "$dependencies" | awk 'NR > 1 {print $1}')

mkdir -p "$OUTPUT_DIR"
OUTPUT_DIR="$(cd "$OUTPUT_DIR" && pwd)"
IMAGE_NAME="Browser-${VERSION}-macos-${ARCH}-${BUILD_SHA:0:7}.dmg"
[[ ! -e "$OUTPUT_DIR/$IMAGE_NAME" ]] || {
    echo "Refusing to replace existing image: $OUTPUT_DIR/$IMAGE_NAME" >&2; exit 1;
}
STAGING="$(mktemp -d "${TMPDIR:-/tmp}/browser-package.XXXXXX")"
MOUNT="$STAGING/mounted"
MOUNTED=0
cleanup() {
    if [[ "$MOUNTED" == 1 ]]; then
        hdiutil detach "$MOUNT" || hdiutil detach -force "$MOUNT" || return 1
    fi
    rm -rf "$STAGING"
}
trap cleanup EXIT
CONTENTS="$STAGING/volume/Browser.app/Contents"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources"
cp "$BINARY" "$CONTENTS/MacOS/Browser"
chmod 755 "$CONTENTS/MacOS/Browser"
cp "$ROOT/LICENSE" "$CONTENTS/Resources/LICENSE.txt"
cp "$ROOT/crates/desktop/assets/Inter-LICENSE.txt" "$CONTENTS/Resources/Inter-LICENSE.txt"
cp "$ROOT/crates/desktop/macos/Info.plist" "$CONTENTS/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$CONTENTS/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $VERSION" "$CONTENTS/Info.plist"
/usr/libexec/PlistBuddy -c "Set :BrowserBuildCommit $BUILD_SHA" "$CONTENTS/Info.plist"
plutil -lint "$CONTENTS/Info.plist"
printf 'APPL????' > "$CONTENTS/PkgInfo"
codesign --force --deep --sign - --timestamp=none "$STAGING/volume/Browser.app"
codesign --verify --deep --strict --verbose=2 "$STAGING/volume/Browser.app"
# Rune's tooling-enabled --help exits before creating a native window. Run the bundled
# executable to catch loader/signature problems that static bundle checks cannot detect.
if ! "$CONTENTS/MacOS/Browser" --help > "$STAGING/launch-help.txt" 2>&1; then
    cat "$STAGING/launch-help.txt" >&2
    echo "The packaged executable failed its command-line launch check." >&2
    exit 1
fi
ln -s /Applications "$STAGING/volume/Applications"
cat > "$STAGING/volume/.Read Me.txt" <<INSTALL_NOTES
Browser $VERSION ($ARCH), commit $BUILD_SHA

Drag Browser.app to Applications, then eject this disk image.
Requires macOS 13 or later.

This testing build is ad-hoc signed, not Developer ID signed or notarized.
macOS may block its first launch. Only if you trust this download, use
System Settings > Privacy & Security > Open Anyway after attempting to open it.

The application and bundled-font licenses are inside Browser.app/Contents/Resources.
INSTALL_NOTES
# Set Finder metadata on the actual volume so background aliases have its real identity.
: "${DMG_LAYOUT_PYTHON:?Set DMG_LAYOUT_PYTHON to Python with macos-dmg-requirements.txt installed}"
hdiutil create -volname "Browser $VERSION" -srcfolder "$STAGING/volume" \
    -format UDRW -fs HFS+ "$STAGING/writable.dmg"
mkdir -p "$MOUNT"
hdiutil attach -nobrowse -mountpoint "$MOUNT" "$STAGING/writable.dmg"
MOUNTED=1
mkdir -p "$MOUNT/.background"
swift "$ROOT/scripts/dmg-background.swift" "$MOUNT/.background/background.png"
"$DMG_LAYOUT_PYTHON" "$ROOT/scripts/dmg-layout.py" "$MOUNT"
hdiutil detach "$MOUNT"
MOUNTED=0
hdiutil convert "$STAGING/writable.dmg" -format UDZO -o "$STAGING/$IMAGE_NAME"
hdiutil verify "$STAGING/$IMAGE_NAME"

# Validate the shipped image, not just the staging folder. Also reproduce copying to Applications.
hdiutil attach -readonly -nobrowse -mountpoint "$MOUNT" "$STAGING/$IMAGE_NAME"
MOUNTED=1
"$DMG_LAYOUT_PYTHON" "$ROOT/scripts/dmg-layout.py" --verify-only "$MOUNT"
[[ "$(readlink "$MOUNT/Applications")" == /Applications ]]
[[ -s "$MOUNT/Browser.app/Contents/_CodeSignature/CodeResources" ]]
plutil -lint "$MOUNT/Browser.app/Contents/Info.plist"
codesign --verify --deep --strict --verbose=2 "$MOUNT/Browser.app"
mkdir -p "$STAGING/installed"
ditto "$MOUNT/Browser.app" "$STAGING/installed/Browser.app"
codesign --verify --deep --strict --verbose=2 "$STAGING/installed/Browser.app"
"$STAGING/installed/Browser.app/Contents/MacOS/Browser" --help > "$STAGING/installed-help.txt" 2>&1
hdiutil detach "$MOUNT"
MOUNTED=0
mv "$STAGING/$IMAGE_NAME" "$OUTPUT_DIR/$IMAGE_NAME"
(
    cd "$OUTPUT_DIR"
    shasum -a 256 "$IMAGE_NAME" > "$IMAGE_NAME.sha256"
)
echo "Packaged $OUTPUT_DIR/$IMAGE_NAME"
