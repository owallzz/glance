#!/usr/bin/env bash
# Build glance.AppImage from a release binary (run on Linux x86_64).
#   scripts/package-appimage.sh <glance-binary> <out.AppImage>
# Uses linuxdeploy ($LINUXDEPLOY, or downloaded into a temp dir). Core
# system libraries stay on the host: glibc, GL/Vulkan, fontconfig and
# friends via linuxdeploy's excludelist, plus PipeWire, ALSA, D-Bus and the
# C++ runtime, which must match the host. Without FUSE, set
# APPIMAGE_EXTRACT_AND_RUN=1.
set -euo pipefail
bin=$(realpath "$1")
out=$(realpath -m "$2")
id=io.github.owallzz.glance
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

ld=${LINUXDEPLOY:-}
if [ -z "$ld" ]; then
  ld="$work/linuxdeploy-x86_64.AppImage"
  curl -fsSLo "$ld" https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage
  chmod +x "$ld"
fi

app="$work/AppDir"
mkdir -p "$app/usr/share/metainfo"
cp "$root/packaging/linux/$id.metainfo.xml" "$app/usr/share/metainfo/$id.appdata.xml"
"$bin" --export-icons "$work/png"
icons=()
for s in 16 32 48 64 128 256 512; do
  mkdir -p "$work/icons/${s}"
  cp "$work/png/icon_${s}.png" "$work/icons/${s}/$id.png"
  icons+=(--icon-file "$work/icons/${s}/$id.png")
done

cd "$work"
ARCH=x86_64 LDAI_OUTPUT="$out" OUTPUT="$out" "$ld" \
  --appdir "$app" \
  --executable "$bin" \
  --desktop-file "$root/packaging/linux/$id.desktop" \
  "${icons[@]}" \
  --exclude-library 'libpipewire-0.3.so*' \
  --exclude-library 'libasound.so*' \
  --exclude-library 'libdbus-1.so*' \
  --exclude-library 'libvulkan.so*' \
  --exclude-library 'libstdc++.so*' \
  --exclude-library 'libgcc_s.so*' \
  --output appimage
chmod +x "$out"
echo "$out"
