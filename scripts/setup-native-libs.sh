#!/usr/bin/env bash
# Immutable Fedora variants ship the runtime sonames for these libraries but not
# the unversioned `.so` developer symlinks the linker needs. This creates a
# private `native/lib` directory of symlinks (gitignored). No system files are
# touched, and the binary still loads the distribution's libraries at runtime.
# On a normal distro with the -devel packages installed you don't need this.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p native/lib
link() {
  local name="$1" found
  found=$(ldconfig -p | awk -v n="$name" '$1 == n && f == "" { f = $NF } END { print f }')
  if [[ -z "$found" ]]; then
    echo "missing runtime library: $name" >&2
    exit 1
  fi
  ln -sf "$found" "native/lib/${name%.so.*}.so"
}
for lib in libxcb.so.1 libxkbcommon.so.0 libxkbcommon-x11.so.0 libfontconfig.so.1 libfreetype.so.6 libwayland-client.so.0 libpipewire-0.3.so.0 libdbus-1.so.3 libX11.so.6 libxcb-shm.so.0 libxcb-randr.so.0 libXrandr.so.2; do
  link "$lib"
done
echo "native/lib ready"
