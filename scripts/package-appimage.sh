#!/usr/bin/env bash
# Build glance.AppImage from a release binary (run on Linux x86_64).
#   scripts/package-appimage.sh <glance-binary> <out.AppImage>
# Uses linuxdeploy ($LINUXDEPLOY, or a pinned release downloaded into a
# temp dir and checked against its sha256). Core
# system libraries stay on the host: glibc, GL/Vulkan, fontconfig and
# friends via linuxdeploy's excludelist, plus PipeWire, ALSA, D-Bus and the
# C++ runtime, which must match the host. Without FUSE, set
# APPIMAGE_EXTRACT_AND_RUN=1. Nothing is stripped (NO_STRIP): the bundled
# libraries already are, and linuxdeploy's strip is too old for some hosts
# (.relr.dyn).
set -euo pipefail
bin=$(realpath "$1")
out=$(realpath -m "$2")
id=io.github.owallzz.glance
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# A fixed linuxdeploy release, checked against its hash before it runs
# (it runs in the release job). To upgrade, pick a new tag and update both.
ld_tag=1-alpha-20251107-1
ld_sha256=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d
ld=${LINUXDEPLOY:-}
if [ -z "$ld" ]; then
  ld="$work/linuxdeploy-x86_64.AppImage"
  curl -fsSLo "$ld" "https://github.com/linuxdeploy/linuxdeploy/releases/download/$ld_tag/linuxdeploy-x86_64.AppImage"
  echo "$ld_sha256  $ld" | sha256sum -c --quiet -
  chmod +x "$ld"
fi
# The AppImage runtime (the loader at the start of the AppImage), pinned the
# same way; otherwise appimagetool fetches its moving "continuous" build.
rt_tag=20251108
rt_sha256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
runtime="$work/runtime-x86_64"
curl -fsSLo "$runtime" "https://github.com/AppImage/type2-runtime/releases/download/$rt_tag/runtime-x86_64"
echo "$rt_sha256  $runtime" | sha256sum -c --quiet -

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
NO_STRIP=1 ARCH=x86_64 LDAI_RUNTIME_FILE="$runtime" LDAI_OUTPUT="$out" OUTPUT="$out" "$ld" \
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
