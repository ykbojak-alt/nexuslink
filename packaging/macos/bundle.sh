#!/usr/bin/env bash
# Build NexusLink.app + nexuslink-<ver>-macos-universal.dmg
# Usage: packaging/macos/bundle.sh <path-to-universal-binary> <version>
# Runs entirely on system tools: lipo, sips, iconutil, hdiutil.
set -euo pipefail

BIN="${1:?usage: bundle.sh <universal-binary> <version>}"
VERSION="${2:-0.2.0}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="$ROOT/target/bundle"
APP="$OUT/NexusLink.app"

rm -rf "$OUT"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# --- binary + plist -------------------------------------------------------
cp "$BIN" "$APP/Contents/MacOS/nexuslink"
chmod +x "$APP/Contents/MacOS/nexuslink"
sed "s/@VERSION@/$VERSION/g" "$ROOT/packaging/macos/Info.plist" > "$APP/Contents/Info.plist"
echo 'APPL????' > "$APP/Contents/PkgInfo"

# --- icon: png -> iconset -> icns -----------------------------------------
ICONSET="$OUT/AppIcon.iconset"
mkdir -p "$ICONSET"
SRC="$ROOT/assets/nexuslink-512.png"
for spec in "16 16" "32 32" "128 128" "256 256" "512 512"; do
  set -- $spec
  sips -z "$2" "$2" "$SRC" --out "$ICONSET/icon_${1}x${1}.png" >/dev/null
  sips -z "$((2 * $2))" "$((2 * $2))" "$SRC" --out "$ICONSET/icon_${1}x${1}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"

# --- dmg ------------------------------------------------------------------
DMG="$OUT/nexuslink-$VERSION-macos-universal.dmg"
hdiutil create -volname "NexusLink $VERSION" -srcfolder "$APP" -ov -format UDZO "$DMG"

echo "built: $APP"
echo "built: $DMG"
