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
lipo -verify_arch "$ARCH" "$BINARY"

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
trap 'rm -rf "$STAGING"' EXIT
CONTENTS="$STAGING/volume/Browser.app/Contents"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources"
cp "$BINARY" "$CONTENTS/MacOS/Browser"
chmod 755 "$CONTENTS/MacOS/Browser"
cp "$ROOT/LICENSE" "$CONTENTS/Resources/LICENSE.txt"
cp "$ROOT/crates/desktop/assets/Inter-LICENSE.txt" "$CONTENTS/Resources/Inter-LICENSE.txt"
cat > "$CONTENTS/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>com.lucidsoftworks.browser</string>
    <key>CFBundleName</key><string>Browser</string>
    <key>CFBundleDisplayName</key><string>Browser</string>
    <key>CFBundleExecutable</key><string>Browser</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
    <key>BrowserBuildCommit</key><string>$BUILD_SHA</string>
</dict>
</plist>
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
cat > "$STAGING/volume/Read Me.txt" <<EOF
Browser $VERSION ($ARCH), commit $BUILD_SHA

Drag Browser.app to Applications, then eject this disk image.
Requires macOS 13 or later.

This testing build is ad-hoc signed, not Developer ID signed or notarized.
macOS may block its first launch. Only if you trust this download, use
System Settings > Privacy & Security > Open Anyway after attempting to open it.

The application and bundled-font licenses are inside Browser.app/Contents/Resources.
EOF
hdiutil create -volname "Browser $VERSION" -srcfolder "$STAGING/volume" \
    -format UDZO -fs HFS+ "$STAGING/$IMAGE_NAME"
hdiutil verify "$STAGING/$IMAGE_NAME"
mv "$STAGING/$IMAGE_NAME" "$OUTPUT_DIR/$IMAGE_NAME"
(
    cd "$OUTPUT_DIR"
    shasum -a 256 "$IMAGE_NAME" > "$IMAGE_NAME.sha256"
)
echo "Packaged $OUTPUT_DIR/$IMAGE_NAME"
