#!/usr/bin/env bash
# Build glance.app and a .dmg from a release binary (run on macOS).
#   scripts/package-macos.sh <glance-binary> <version> <out-dir>
# Leaves <out-dir>/glance.dmg and <out-dir>/glance.app.
# The app is ad-hoc signed (no Apple Developer ID): the first launch of a
# downloaded copy needs right-click > Open.
set -euo pipefail
bin=$1 version=$2 out=$3
work=$(mktemp -d)
app="$work/glance.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$out"
cp "$bin" "$app/Contents/MacOS/glance"
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
# Icon: render the PNGs from the app itself, then build an .icns.
iconset="$work/glance.iconset"
mkdir -p "$iconset"
"$bin" --export-icons "$work/png"
for s in 16 32 128 256 512; do
  cp "$work/png/icon_${s}.png" "$iconset/icon_${s}x${s}.png"
  cp "$work/png/icon_$((s * 2)).png" "$iconset/icon_${s}x${s}@2x.png"
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/glance.icns"
codesign --force --deep --sign - "$app"
codesign --verify --deep "$app"
# Disk image with the app and a shortcut to Applications.
stage="$work/dmg"
mkdir -p "$stage"
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
hdiutil create -volname glance -srcfolder "$stage" -ov -format UDZO "$out/glance.dmg" >/dev/null
# The bare bundle too, for the .app.tar.gz the updater installs.
rm -rf "$out/glance.app"
cp -R "$app" "$out/"
echo "$out/glance.dmg"
